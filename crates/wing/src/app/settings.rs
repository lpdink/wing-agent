//! 设置面板的 App 侧接线（`/settings`）。
//!
//! 07 的状态机（`shared/panels/settings`）产出 [`SettingsAction`]，08 的 overlay
//! （`ui/settings`）把它画出来；这一层负责**它周围的一切**：
//!
//! * **生命周期**——打开（cache-first + 后台刷新）、关闭（预览回退 + 整屏重画 +
//!   图片失效，design §20 风险 13）、`R` 重载、`Ctrl+R` 重启；
//! * **实时预览**（design §15.3）——Interface 根的任何提交立刻换掉 `self.config` /
//!   `self.palette` 并整屏重画；打开时记快照，`Esc` 放弃 / `R` 重载都回到它；
//! * **一键保存两边**（design §15.4）——Interface 半边本地原子写、Gateway 半边异步
//!   POST，两边各自成败、合并成一条回执 notice；
//! * **`settings_changed` 的消费**（design §7.6）——指纹比对，绝不"猜是不是自己"。
//!
//! 这里是 `app/**` 里唯一允许写设置面板细节的地方：`modal.rs` 只做"喂按键 + 分派"，
//! `runner.rs` 只做 I/O 的形状，`commands.rs` 只做路由。

use std::time::Duration;

use serde_json::Value;
use wing_api_client::models::SettingsGetResponse;
use wing_api_client::models::SettingsSchemaResponse;
use wing_api_client::models::SettingsSetResponse;

use super::App;
use super::AppIntent;
use super::transport::GatewayEndpoint;
use super::transport::Transport;
use super::transport::connect_transport;
use crate::config::AppConfig;
use crate::config::ThemePalette;
use crate::shared::panels::settings::InterfaceSource;
use crate::shared::panels::settings::SaveOutcome;
use crate::shared::panels::settings::SettingsAction;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::View;
use crate::ui::chat_view::ChatCell;
use crate::ui::toast::Toast;

/// 一次保存里 **Interface 半边**（本地写盘）的结论。
///
/// Gateway 半边异步回来后才合并回执，所以这个结论要先存在
/// [`App`] 上（见 [`PendingSettingsSave`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InterfaceSaveReport {
    /// 本次没有 Interface 改动（没尝试写盘）。
    Skipped,
    /// 写盘成功：`changed` = 相对磁盘旧内容的叶子路径 diff 条数。
    Ok { path: String, changed: usize },
    /// 写盘失败（备份 / 建目录 / 替换任一步）。
    Err { message: String },
}

impl InterfaceSaveReport {
    /// 尝试过这个半边吗（回执只在尝试过的半边里数成败）。
    fn attempted(&self) -> bool {
        !matches!(self, Self::Skipped)
    }

    fn ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// 没尝试或成功了 —— 回执的"这一边没拖后腿"。
    fn ok_or_skipped(&self) -> bool {
        !self.attempted() || self.ok()
    }
}

/// 一次保存里 **Gateway 半边**的结论。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum GatewaySaveReport {
    /// 本次没有 Gateway 改动（没发请求）。
    Skipped,
    /// `POST /api/settings/set` 拿到了响应（`ok` 可能是 `false` + problems）。
    Saved(SettingsSetResponse),
    /// 请求本身失败：`conflict` = 409（指纹冲突）。
    Failed { message: String, conflict: bool },
}

impl GatewaySaveReport {
    fn attempted(&self) -> bool {
        !matches!(self, Self::Skipped)
    }

    fn ok(&self) -> bool {
        matches!(self, Self::Saved(response) if response.ok)
    }

    /// 没尝试或成功了 —— 回执的"这一边没拖后腿"。
    fn ok_or_skipped(&self) -> bool {
        !self.attempted() || self.ok()
    }
}

/// 等待 Gateway 回执的那一次保存：Interface 半边的结论 + 本次发出的文档
/// （`ok=true` 时写回缓存，让下次打开面板仍能 cache-first）。
#[derive(Debug, Clone)]
pub(super) struct PendingSettingsSave {
    pub(super) interface: InterfaceSaveReport,
    pub(super) document: Value,
}

impl App {
    // -----------------------------------------------------------------------
    // 生命周期
    // -----------------------------------------------------------------------

    /// `/settings`（别名 `/config` / `/set`）：打开面板。
    ///
    /// cache-first（照 `/model` 的模式）：有上一次成功拉取就立刻开面板、同时推一次
    /// 后台刷新；没有再推一次拉取，结果到达时自动打开（[`App::handle_settings_fetch`]）。
    /// **允许在轮次进行中打开**（design §12.5）。
    pub(super) fn open_settings_panel(&mut self) {
        if self.settings_panel.is_some() {
            return;
        }
        if self.settings_cache.is_none() {
            self.show_toast(Toast::info("正在载入设置…", Duration::from_secs(3)));
        }
        // 打开时的 config 快照：`Esc` 放弃 / `R` 重载的回退基准。
        self.config_snapshot = Some(self.config.clone());
        self.settings_pending = true;
        self.push_intent(AppIntent::FetchSettings);
        if let Some((schema, state)) = self.settings_cache.clone() {
            self.present_settings_panel(&schema, state);
        }
    }

    /// 面板真的出现的时刻：构造 + 注入 Interface 根 + 取消选区。
    ///
    /// Interface 根的读失败**不**用默认值冒充（09 的口径：读失败 ≠ 默认值）——
    /// 树里就没有 Interface 根，`s` 也不会去写那个文件（`interface_dirty` 恒 false）。
    fn present_settings_panel(
        &mut self,
        schema: &SettingsSchemaResponse,
        state: SettingsGetResponse,
    ) {
        let catalog = crate::config::catalog::interface_catalog();
        let interface = match crate::config::store::read_interface_doc() {
            Ok(read) => {
                self.interface_catalog = Some(catalog.clone());
                Some(InterfaceSource {
                    catalog,
                    doc: read.doc,
                })
            }
            Err(e) => {
                self.interface_catalog = None;
                self.show_toast(Toast::warning(
                    format!("无法读取 TUI 配置，面板只显示 Gateway 根：{e}"),
                    Duration::from_secs(5),
                ));
                None
            }
        };
        self.settings_panel = Some(SettingsPanel::new(schema, state, interface, View::Tree));
        self.settings_pending = false;
        // 选区锚在内容坐标：overlay 期间的帧几何与内容都变了，留着只会指向别的文本。
        self.cancel_selection();
        self.chat_dirty = true;
    }

    /// 关闭面板（`discard=true` = 用户确认放弃未保存改动 → 预览回退）。
    ///
    /// **无论哪条路径**都要整屏重画 + 让图片句柄失效：overlay 的 `Clear` 抹过终端，
    /// ratatui 的 diff 面对"曾经被 Clear 过"的屏幕会留下残影（design §20 风险 13），
    /// 而图片协议的内容在 Clear 之后终端已不再持有。
    pub(super) fn close_settings_panel(&mut self, discard: bool) {
        self.settings_panel = None;
        self.interface_catalog = None;
        self.settings_pending = false;
        self.settings_reload_pending = false;
        if discard {
            self.restore_config_snapshot();
        }
        // 面板开着时新起的拖拽锚在看不见的 chat band 上：一起收掉。
        self.cancel_selection();
        self.needs_full_redraw = true;
        self.images.invalidate();
        self.chat_dirty = true;
    }

    /// 测试专用的打开态：与 [`App::present_settings_panel`] 同形，但 Interface 文档由
    /// 调用方给定 —— 单测不该读（也不该依赖）真实的 `~/.wing/tui/config.yaml`。
    #[cfg(test)]
    pub(crate) fn inject_settings_panel(
        &mut self,
        schema: &SettingsSchemaResponse,
        state: SettingsGetResponse,
        interface: Option<InterfaceSource>,
    ) {
        self.config_snapshot = Some(self.config.clone());
        self.interface_catalog = interface.as_ref().map(|source| source.catalog.clone());
        self.settings_panel = Some(SettingsPanel::new(
            schema,
            state.clone(),
            interface,
            View::Tree,
        ));
        self.settings_cache = Some((schema.clone(), state));
        self.cancel_selection();
    }

    /// `FetchPayload::Settings` 的落地：打开 / 后台刷新 / `R` 重载三个来源共用。
    ///
    /// 三条口径写死在这里：
    /// * 面板开着 + `R` 重载 → **无条件** `apply_snapshot`（丢弃本地改动是它的语义）；
    /// * 面板开着 + 后台刷新 → 只在面板还干净时应用（否则会抹掉用户刚敲的值）；
    /// * 面板关着 + 有在飞的首次拉取（`settings_pending`）→ 现在打开。
    pub(super) fn handle_settings_fetch(
        &mut self,
        schema: SettingsSchemaResponse,
        state: SettingsGetResponse,
    ) {
        let reload = std::mem::take(&mut self.settings_reload_pending);
        let open_pending = std::mem::take(&mut self.settings_pending);
        self.settings_cache = Some((schema.clone(), state.clone()));
        if let Some(panel) = self.settings_panel.as_mut() {
            if reload || panel.dirty_count() == 0 {
                panel.apply_snapshot(&schema, state);
            }
        } else if open_pending {
            self.present_settings_panel(&schema, state);
        }
    }

    // -----------------------------------------------------------------------
    // 实时预览（design §15.3）
    // -----------------------------------------------------------------------

    /// Interface 根的任何提交：候选文档 → `self.config` + 调色板 + 整屏重画。
    pub(super) fn apply_settings_preview(&mut self, doc: &Value) {
        self.config = crate::config::store::appconfig_from_doc(doc);
        self.apply_palette();
    }

    /// `Close{discard:true}` / `R` 重载的本地半边：`self.config` 回到快照。
    pub(super) fn restore_config_snapshot(&mut self) {
        let Some(snapshot) = self.config_snapshot.clone() else {
            return;
        };
        self.config = snapshot;
        self.apply_palette();
    }

    /// 新的 `self.config` → 调色板 / composer 行数 / 全部渲染缓存作废 / 整屏重画。
    ///
    /// 调色板**不在任何一层渲染缓存的键里**（`CellContext` 是呈现参数，与 Ctrl+O
    /// 翻转思考块同一个问题）：不显式作废，聊天区的旧颜色会一直留到内容变化为止。
    fn apply_palette(&mut self) {
        self.palette = ThemePalette::from_config(&self.config.colors);
        self.input.max_lines = self.config.layout.max_input_lines;
        self.chat.invalidate_cells();
        self.welcome_theme_dirty = true;
        self.needs_full_redraw = true;
    }

    // -----------------------------------------------------------------------
    // 动作分派（07 → App）
    // -----------------------------------------------------------------------

    /// 把状态机的一个 [`SettingsAction`] 落成 App 的动作（design §12.4 的分派表）。
    pub(super) fn handle_settings_action(&mut self, action: SettingsAction) {
        match action {
            SettingsAction::None => {}
            SettingsAction::PreviewInterface(doc) => self.apply_settings_preview(&doc),
            SettingsAction::Save {
                gateway,
                base,
                interface,
                gateway_dirty,
                interface_dirty,
            } => {
                self.push_intent(AppIntent::SaveSettings {
                    gateway: Box::new(gateway),
                    base,
                    interface: Box::new(interface),
                    gateway_dirty,
                    interface_dirty,
                });
            }
            SettingsAction::RestartGateway => {
                // §12.5：只有"立即重启网关"在轮次进行中被拒绝（面板只产出意图）。
                if self.turn.working {
                    self.show_toast(Toast::warning(
                        "正在运行中，无法重启网关",
                        Duration::from_secs(4),
                    ));
                    return;
                }
                self.show_toast(Toast::info("正在重启网关…", Duration::from_secs(10)));
                self.push_intent(AppIntent::RestartGateway);
            }
            SettingsAction::Close { discard } => self.close_settings_panel(discard),
            SettingsAction::Reload => self.reload_settings(),
        }
    }

    /// `R`：丢弃本地改动并重拉。
    ///
    /// 本地半边立刻做（预览回到磁盘状态），Gateway 半边走 `ReloadSettings` ——
    /// 回到 `get` 的结果由 `settings_reload_pending` 标记为"无条件应用"。
    pub(super) fn reload_settings(&mut self) {
        let catalog = self.interface_catalog.clone();
        match (catalog, crate::config::store::read_interface_doc()) {
            (Some(catalog), Ok(read)) => {
                let doc = read.doc;
                if let Some(panel) = self.settings_panel.as_mut() {
                    panel.set_interface(InterfaceSource {
                        catalog,
                        doc: doc.clone(),
                    });
                }
                // 重载后的回退基准 = 磁盘上真实的内容。
                let config = crate::config::store::appconfig_from_doc(&doc);
                self.config = config.clone();
                self.config_snapshot = Some(config);
                self.apply_palette();
            }
            (_, Err(e)) => {
                // 磁盘读不动：预览照旧回退到快照，Interface 根保持面板里的样子。
                self.restore_config_snapshot();
                self.show_toast(Toast::warning(
                    format!("无法读取 TUI 配置：{e}"),
                    Duration::from_secs(4),
                ));
            }
            (None, Ok(_)) => self.restore_config_snapshot(),
        }
        self.settings_reload_pending = true;
        self.push_intent(AppIntent::ReloadSettings);
    }

    // -----------------------------------------------------------------------
    // 保存的两半（design §15.4）
    // -----------------------------------------------------------------------

    /// Interface 半边的结论落地：成功 → 面板清脏 / 记基线 + **快照更新**
    /// （否则再按 `Esc` 会把已保存的改动"撤销"掉）；失败 → 面板保留脏标记。
    pub(super) fn settle_interface_save(
        &mut self,
        report: &InterfaceSaveReport,
        saved_doc: &Value,
    ) {
        match report {
            InterfaceSaveReport::Skipped => {}
            InterfaceSaveReport::Ok { .. } => {
                if let Some(panel) = self.settings_panel.as_mut() {
                    panel.apply_save(SaveOutcome {
                        gateway: None,
                        interface_ok: Some(true),
                    });
                }
                self.config_snapshot = Some(crate::config::store::appconfig_from_doc(saved_doc));
            }
            InterfaceSaveReport::Err { .. } => {
                if let Some(panel) = self.settings_panel.as_mut() {
                    panel.apply_save(SaveOutcome {
                        gateway: None,
                        interface_ok: Some(false),
                    });
                }
            }
        }
    }

    /// Gateway 半边的结论落地：面板 `apply_save` + 缓存回写 + 合并回执。
    ///
    /// 两半的汇合点。`settings_save` 里存着 Interface 半边的结论（短路路径在调用前
    /// 也会把它放好），所以合并只看这一份状态。
    pub(super) fn settle_gateway_save(&mut self, gateway: GatewaySaveReport) {
        let pending = self.settings_save.take();
        let interface = pending
            .as_ref()
            .map(|pending| pending.interface.clone())
            .unwrap_or(InterfaceSaveReport::Skipped);

        match &gateway {
            GatewaySaveReport::Saved(response) => {
                if let Some(panel) = self.settings_panel.as_mut() {
                    // `ok=false` 时面板自己会切到问题清单视图（07 的 apply_save）。
                    panel.apply_save(SaveOutcome {
                        gateway: Some(response.clone()),
                        interface_ok: None,
                    });
                }
                if let Some((_, cached)) = self.settings_cache.as_mut() {
                    cached.fingerprint = response.fingerprint.clone();
                    cached.problems = response.problems.clone();
                    // `ok=false` 时文件没变：缓存里的值照旧，只换问题。
                    if response.ok
                        && let Some(pending) = &pending
                    {
                        cached.values = pending.document.clone();
                    }
                }
            }
            GatewaySaveReport::Failed { conflict: true, .. } => self.mark_settings_stale(),
            GatewaySaveReport::Failed { .. } | GatewaySaveReport::Skipped => {}
        }

        self.show_settings_save_notice(&interface, &gateway);
    }

    /// 409（指纹冲突）：面板顶部横幅「按 R 重新载入」，**不覆盖**本地改动。
    ///
    /// 409 的响应体里没有新指纹，而 [`SettingsPanel::on_settings_changed`] 的语义是
    /// 指纹比对 —— 传一个不可能等于真指纹的空串，让比对必然为"不同"。
    pub(super) fn mark_settings_stale(&mut self) {
        if let Some(panel) = self.settings_panel.as_mut() {
            panel.on_settings_changed("");
        }
    }

    /// 保存回执：transcript 里的一条 notice（`NoticeRow` 的 rail 形态）+
    /// 一条摘要 toast（面板是全屏的，回执在它底下看不见）。
    pub(super) fn show_settings_save_notice(
        &mut self,
        interface: &InterfaceSaveReport,
        gateway: &GatewaySaveReport,
    ) {
        let text = save_notice_text(interface, gateway);
        let ok = save_notice_ok(interface, gateway);
        if ok {
            self.chat.push(ChatCell::SystemMessage(text));
        } else {
            self.chat.push(ChatCell::WarningMessage(text));
        }
        let toast = save_notice_toast(interface, gateway);
        let duration = Duration::from_secs(if ok { 3 } else { 5 });
        if ok {
            self.show_toast(Toast::info(toast, duration));
        } else {
            self.show_toast(Toast::warning(toast, duration));
        }
    }

    // -----------------------------------------------------------------------
    // settings_changed 的消费（design §7.6）
    // -----------------------------------------------------------------------

    /// `settings_changed` 事件：面板开着 → 指纹比对（相同 = 自己刚保存的那一次，
    /// 不警告；不同 = 别人改了，横幅）；关着 → 丢掉缓存。
    ///
    /// 丢缓存而不是只改指纹：缓存里的 `values` 与 `fingerprint` 必须同源，
    /// 只更新指纹会造出"旧文档 + 新指纹"的 base —— 下次保存会把别人的改动覆盖掉。
    pub(super) fn note_settings_changed(&mut self, fingerprint: &str) {
        if let Some(panel) = self.settings_panel.as_mut() {
            panel.on_settings_changed(fingerprint);
        } else {
            self.settings_cache = None;
        }
    }
}

// ---------------------------------------------------------------------------
// 回执文案（纯函数：四组合逐条有测试）
// ---------------------------------------------------------------------------

/// 合并回执的正文（不含 rail；`ChatCell` 渲染时由 `NoticeRow` 加栏杆）。
pub(super) fn save_notice_text(
    interface: &InterfaceSaveReport,
    gateway: &GatewaySaveReport,
) -> String {
    let attempted = usize::from(interface.attempted()) + usize::from(gateway.attempted());
    let failed = usize::from(interface.attempted() && !interface.ok())
        + usize::from(gateway.attempted() && !gateway.ok());
    let mut lines: Vec<String> = vec![notice_header(attempted, failed).to_string()];

    if let Some(line) = interface_line(interface) {
        lines.push(line);
    }
    if let Some(line) = gateway_line(gateway) {
        lines.push(line);
    }
    // 需重启的提示：AD1 的文案（`Ctrl+R`，不是 `r`），只在 Gateway 真的保存成功时出现。
    if let GatewaySaveReport::Saved(response) = gateway
        && response.ok
        && !response.restart_required.is_empty()
    {
        lines.push(format!(
            "  ⚠ {} 需重启网关才生效 — Ctrl+R 立即重启",
            response.restart_required.join("、")
        ));
    }
    // 后端的非致命说明（AD13）：当前文件不可解析时，密文无从回填 —— 必须**逐条**
    // 让用户看见（"其中旧密钥没有保留，请重新填写"）。它不影响成败等级。
    if let GatewaySaveReport::Saved(response) = gateway {
        for warning in &response.warnings {
            lines.push(format!("  ⚠ {warning}"));
        }
    }
    lines.join("\n")
}

/// 回执的成败口径（决定 notice 的 rail 等级与 toast）。
pub(super) fn save_notice_ok(interface: &InterfaceSaveReport, gateway: &GatewaySaveReport) -> bool {
    interface.ok_or_skipped() && gateway.ok_or_skipped()
}

fn notice_header(attempted: usize, failed: usize) -> &'static str {
    if failed == 0 {
        "设置已保存"
    } else if failed == attempted {
        "设置未保存"
    } else {
        "设置部分保存"
    }
}

fn interface_line(report: &InterfaceSaveReport) -> Option<String> {
    match report {
        InterfaceSaveReport::Skipped => None,
        InterfaceSaveReport::Ok { path, changed } => {
            Some(format!("  ✓ {:<12}{path}（{changed} 项）", "Interface"))
        }
        InterfaceSaveReport::Err { message } => {
            Some(format!("  ✗ {:<12}写入失败：{message}", "Interface"))
        }
    }
}

fn gateway_line(report: &GatewaySaveReport) -> Option<String> {
    match report {
        GatewaySaveReport::Skipped => None,
        GatewaySaveReport::Saved(response) if response.ok => Some(format!(
            "  ✓ {:<12}{}",
            "Gateway",
            gateway_success_summary(response)
        )),
        GatewaySaveReport::Saved(response) => Some(format!(
            "  ✗ {:<12}{} 个问题，未保存（已切到问题清单）",
            "Gateway",
            response.problems.len()
        )),
        GatewaySaveReport::Failed { conflict: true, .. } => Some(format!(
            "  ✗ {:<12}配置已被其它客户端修改，按 R 重新载入",
            "Gateway"
        )),
        GatewaySaveReport::Failed { message, .. } => {
            Some(format!("  ✗ {:<12}保存失败：{message}", "Gateway"))
        }
    }
}

/// `N 项变更 · <热重载逐项>`（回执的第二列）。
fn gateway_success_summary(response: &SettingsSetResponse) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !response.changed.is_empty() {
        parts.push(format!("{} 项变更", response.changed.len()));
    }
    if let Some(reload) = &response.reload {
        let mut reloaded: Vec<String> = Vec::new();
        let mut failed = 0usize;
        for item in &reload.results {
            if item.ok {
                reloaded.push(match item.detail.as_deref() {
                    Some(detail) if !detail.is_empty() => format!("{} {detail}", item.name),
                    _ => item.name.clone(),
                });
            } else {
                failed += 1;
            }
        }
        if !reloaded.is_empty() {
            parts.push(reloaded.join(" · "));
        }
        if failed > 0 {
            parts.push(format!("{failed} 项重载失败"));
        }
    }
    if parts.is_empty() {
        "已应用".to_string()
    } else {
        parts.join(" · ")
    }
}

/// 摘要 toast：面板还没关，回执在它底下看不见 —— "保存到底成没成"要立刻可见。
fn save_notice_toast(interface: &InterfaceSaveReport, gateway: &GatewaySaveReport) -> String {
    if save_notice_ok(interface, gateway) {
        // 有后端 warnings 时不能只说"已保存"：那些话（AD13 的"旧密钥没有保留"）
        // 是用户必须知道的，而回执此刻正压在面板下面。
        if let GatewaySaveReport::Saved(response) = gateway
            && !response.warnings.is_empty()
        {
            return format!("设置已保存 · {} 条说明（见回执）", response.warnings.len());
        }
        return "设置已保存".to_string();
    }
    match gateway {
        GatewaySaveReport::Saved(response) if !response.ok => {
            format!("设置未保存：{} 个问题", response.problems.len())
        }
        GatewaySaveReport::Failed { conflict: true, .. } => {
            "配置已被其它客户端修改，按 R 重新载入".to_string()
        }
        _ => "设置未保存（见回执）".to_string(),
    }
}

/// 两份稀疏文档之间**叶子**路径的差异（新增 / 删除 / 改值都算一条）。
///
/// Interface 半边的「N 项变更」用它算：面板的脏集合是它私有的事实，而这里要的是
/// "磁盘上真的变了什么"——和 Gateway 回执的 `changed` 同一个口径。对象递归下钻；
/// 数组与标量整体算一条（列表的增删改都是一条）。
pub(super) fn changed_leaf_paths(previous: Option<&Value>, next: &Value) -> Vec<String> {
    // 旧文档不存在 ≈ 空文档：整份文档的叶子逐条算"新增"（与"文件不存在时按空文件
    // 比较"同一个口径）。
    let empty = Value::Object(serde_json::Map::new());
    let before = previous.unwrap_or(&empty);
    let mut out = Vec::new();
    collect_changed(before, next, "", &mut out);
    out.sort();
    out
}

fn collect_changed(before: &Value, after: &Value, prefix: &str, out: &mut Vec<String>) {
    if let (Value::Object(before), Value::Object(after)) = (before, after) {
        let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            let child = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match (before.get(key), after.get(key)) {
                // 两边都在：继续下钻（标量会在下一层按值比较）。
                (Some(before), Some(after)) => collect_changed(before, after, &child, out),
                // 整棵子树的增 / 删：一条。
                _ => out.push(child),
            }
        }
        return;
    }
    if before != after {
        out.push(if prefix.is_empty() {
            "<document>".to_string()
        } else {
            prefix.to_string()
        });
    }
}

// ---------------------------------------------------------------------------
// Ctrl+R：立即重启网关
// ---------------------------------------------------------------------------

/// 重启的等待上限（等不可达 / 单次探测超时）。
const RESTART_DOWN_TIMEOUT: Duration = Duration::from_secs(10);
const RESTART_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// `Ctrl+R`：shutdown → 等不可达 → 重新拉起 → 重连 → 会话恢复。
///
/// 复用既有的两段式连接（[`connect_transport`] + [`Transport::recover_session`]），
/// 不重写任何重连逻辑。两条容易踩的线：
///
/// 1. **端口可能刚被改过**（用户改 `gateway.port` 保存 + `Ctrl+R` 的意义就是这个）：
///    第 3 步重读 `~/.wing/core/config.yaml`（host/port 的事实来源）与 TUI 配置
///    （`api_key` 的生效域是 restart），把新的端点写回 `*endpoint`；
/// 2. **重连退避不与显式重启打架**：重启期间 event loop 停在这里，`select!` 的
///    WS 断开分支不会同时运行；成功 → 传输已换成新的（退避没被触发）；失败 →
///    `*transport = None`，交给既有退避路径（它的目标是 `*endpoint`，也就是新端点）。
pub(super) async fn restart_gateway(
    app: &mut App,
    transport: &mut Option<Transport>,
    endpoint: &mut GatewayEndpoint,
) {
    // 1. 请求关闭（失败只 warn：网关可能已经自己死了，它还是在往下走）。
    match transport.as_ref() {
        Some(transport) => {
            if let Err(e) = transport.http.shutdown().await {
                tracing::warn!("gateway shutdown request failed: {e}");
            }
        }
        None => {
            app.show_toast(Toast::warning(
                "网关未连接，无法重启",
                Duration::from_secs(3),
            ));
            return;
        }
    }

    // 2. 等它真的不可达（有界）。
    if !wait_until_unreachable(endpoint, RESTART_DOWN_TIMEOUT).await {
        app.show_toast(Toast::warning(
            "网关未在 10s 内停止，已取消重启",
            Duration::from_secs(4),
        ));
        return;
    }

    // 3. 重读两份配置 → 新端点（端口 / host / api_key 都可能刚被改过）。
    let backend = crate::cmd::backend_config::read_backend_gateway_config();
    let api_key = AppConfig::load().api_key.filter(|key| !key.is_empty());
    *endpoint = GatewayEndpoint {
        ws_url: format!("ws://{}:{}/ws", backend.host, backend.port),
        http_base: format!("http://{}:{}", backend.host, backend.port),
        api_key,
    };

    // 4. 重新拉起（复用 `wing start` 的守护进程启动）。
    if let Err(e) = crate::cmd::start::start_gateway(&backend.host, backend.port).await {
        tracing::warn!("gateway restart failed: {e:#}");
        app.show_toast(Toast::warning(
            format!("网关重启失败：{e}"),
            Duration::from_secs(5),
        ));
        // 交给既有重连退避（目标 = 刚写回的新端点）。
        *transport = None;
        app.set_connected(false);
        return;
    }

    // 5. 重连 + 会话恢复（与 run loop 的 retry 臂同一对原语）。
    match connect_transport(endpoint).await {
        Ok(new_transport) => {
            let workspace = app.launch_workspace.clone();
            match new_transport
                .recover_session(&app.session_id, workspace.as_deref())
                .await
            {
                Ok(()) => {
                    *transport = Some(new_transport);
                    app.set_connected(true);
                    app.clear_toast();
                    app.show_toast(Toast::info(
                        "网关已重启，会话已恢复",
                        Duration::from_secs(3),
                    ));
                    app.invalidate_session_cache();
                    // 与既有 recovery 路径同一组刷新（会话快照 + 命令表）。
                    app.push_intent(AppIntent::FetchInfo);
                    app.push_intent(AppIntent::FetchCommands);
                    // 面板的 restart_required 该熄了：不脏时 apply_snapshot 会清掉它。
                    app.push_intent(AppIntent::FetchSettings);
                    app.needs_full_redraw = true;
                }
                Err(e) => {
                    tracing::warn!("session recovery after restart failed: {e:#}");
                    app.show_toast(Toast::warning(
                        format!("网关已重启，但会话恢复失败：{e}"),
                        Duration::from_secs(5),
                    ));
                    *transport = None;
                    app.set_connected(false);
                }
            }
        }
        Err(e) => {
            tracing::warn!("reconnect after restart failed: {e:#}");
            app.show_toast(Toast::warning(
                format!("网关已重启，但连接失败：{e}"),
                Duration::from_secs(5),
            ));
            *transport = None;
            app.set_connected(false);
        }
    }
}

/// 轮询直到网关**真的下去了**：health 不可达 **且** 端口不再接受连接。
///
/// 单次探测带超时：在 WSL / 容器里 SYN 可能被静默丢掉（`cmd/start.rs` 的同款注释），
/// 不设上限会把"等 10s"变成"等内核的 TCP 重试窗口"。
///
/// **为什么要看端口**（review N5）：`start_gateway` 起手会做一次裸 TCP 占用检查，
/// 而 uvicorn 优雅关闭的窗口里可能"health 已经不响应、listen socket 还没释放" ——
/// 只看 health 就可能在那个窗口里往下走，于是 `Ctrl+R` 报"端口被占用"（其实旧网关
/// 正在死，再按一次就成功）。这里用与 `start_gateway` **同一条判据**（连接成功 =
/// 占用；拒绝 / 超时 = 空闲），把窗口等过去；等不到就还是走"取消重启"的老路。
async fn wait_until_unreachable(endpoint: &GatewayEndpoint, timeout: Duration) -> bool {
    let Ok(probe) =
        wing_api_client::GatewayClient::new(&endpoint.http_base, endpoint.api_key.as_deref())
    else {
        return true;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let down = match tokio::time::timeout(RESTART_PROBE_TIMEOUT, probe.health()).await {
            Ok(Ok(_)) => false,
            // 连接被拒 / 超时 / 任何错误都算"不可达"。
            _ => true,
        };
        if down && !port_listening(endpoint).await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 端口上还有人在 listen 吗 —— 与 [`crate::cmd::start::start_gateway`] 的占用检查
/// 同一条判据（连上 = 有人在；拒绝 / 超时 = 空闲）。
async fn port_listening(endpoint: &GatewayEndpoint) -> bool {
    let Some((host, port)) = endpoint
        .http_base
        .trim_start_matches("http://")
        .rsplit_once(':')
        .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
    else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            RESTART_PROBE_TIMEOUT,
            tokio::net::TcpStream::connect((host, port)),
        )
        .await,
        Ok(Ok(_))
    )
}
