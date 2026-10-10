//! Setup TUI —— 首次运行 / 配置损坏时的修复循环（design §16，D22）。
//!
//! 它是**在 `App` 之前的一个更小的东西**：没有 session、没有 WS、没有意图队列，
//! 绝不进 `run_app`（`App.session_id` 是 `String` 非 `Option`，处处假定有会话；
//! 硬套会污染几十处——design §16.3 的整个设计就是为了避免这个改动）。
//! 终端生命周期（`init_terminal` / `restore_terminal` / panic hook）归**调用方**
//! （[`crate::cmd::run_tui`]），这里只借 `&mut Terminal<B>` 画帧；于是 setup 阶段的
//! panic 也走同一套恢复序列，不会把终端留在 raw mode。
//!
//! 画面 = 背板 + 前景：
//!
//! ```text
//!            (海鸥站姿)              WING（像素大字）
//!         首次运行 · 需要配置一个模型 provider
//! ┌─ ⚙ Settings ─── ~/.wing/core/config.yaml ─────────────────┐
//! │ 待修复                                                    │
//! │   ❯ 1  providers list cannot be empty                     │
//! │   2    agents list cannot be empty                        │
//! └───────────────────────────────────────────────────────────┘
//! ```
//!
//! 背板复用 [`crate::ui::welcome`] 的**帧数据与绘制原语**（`art` 的字母网格、
//! `sprite` 的半格渲染、`wordmark` 的渐变大字），**不复用** `Welcome` 结构体
//! ——它绑定 chat header 的宽度阶梯与可见性门控。前景就是 10 接好的同一个
//! [`SettingsPanel`] + [`SettingsOverlay`]，首屏 = 问题清单（D11）。
//!
//! 三个前端共用这里的预检（[`preflight_config`] / [`preflight_or_report`]）。
//! **stdout 一个字节都不写**（stdio / ACP 的协议帧通道就是这条 stdout 的语义）：
//! 本模块唯一的打印点是降级报告的 `eprint!`。
#![allow(clippy::print_stderr)]

use std::process::ExitCode;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::Alignment;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use serde_json::Value;
use wing_api_client::ApiClientError;
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::models::SettingNode;
use wing_api_client::models::SettingProblem;
use wing_api_client::models::SettingsGetResponse;
use wing_api_client::models::SettingsSchemaResponse;
use wing_api_client::models::SettingsSetRequest;
use wing_api_client::models::SettingsSetResponse;
use wing_api_client::models::SettingsStatusResponse;

use crate::app::transport::GatewayEndpoint;
use crate::config::AppConfig;
use crate::config::ThemePalette;
use crate::config::store::StoreError;
use crate::config::store::WriteOutcome;
use crate::shared::panels::settings::InterfaceSource;
use crate::shared::panels::settings::Problem;
use crate::shared::panels::settings::SaveOutcome;
use crate::shared::panels::settings::SettingsAction;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::View;
use crate::tui::TermEvent;
use crate::ui::settings::SettingsCatalogs;
use crate::ui::settings::SettingsOverlay;
use crate::ui::settings::tree_viewport_rows;
use crate::ui::shimmer::is_light_theme;
use crate::ui::shimmer::to_rgb;
use crate::ui::welcome::art;
use crate::ui::welcome::sprite;
use crate::ui::welcome::wordmark;

/// `sysexits.h` 的 `EX_CONFIG`：让编排器判别"这是配置问题不是网络问题"（D27 / §16.4）。
pub(crate) const EX_CONFIG: u8 = 78;

/// `s` 成功后停留多久再交还启动链（让用户看见 `✓ 配置就绪，正在启动…`）。
const READY_DWELL: Duration = Duration::from_millis(800);

/// 双击 `Ctrl+C` 的窗口（与 `App::handle_quit_key` 同口径）。
const QUIT_DOUBLE_PRESS: Duration = Duration::from_millis(500);

/// 海鸥在背板里占的终端行数——与 `ui::welcome` 的私有常量 `ART_TERM_ROWS` 同值
/// （两个姿态对齐到同一高度；这里只用到站姿，`PERCHED_IDLE` = 23 像素行 = 13 终端行）。
const ART_TERM_ROWS: usize = 13;

/// 站姿网格顶部的透明余量（`ui::welcome` 的 `PERCHED_TOP_PAD` 同值：抬升不裁头冠）。
const PERCHED_TOP_PAD: usize = 2;

/// 海鸥与 wordmark 之间的空列数（`ui::welcome` 的 `ART_GAP` 同值）。
const ART_GAP: usize = 3;

/// 背板高度：海鸥 13 行 + 说明行 1 行。
const BACKPLATE_ROWS: u16 = ART_TERM_ROWS as u16 + 1;

/// 背板 + 面板都放得下的最小高度（14 + 面板 ≥14）；再矮背板让位（小终端里功能优先）。
const BACKPLATE_MIN_HEIGHT: u16 = 28;

/// 背板的最小宽度（再窄只剩说明行，没有画的意义——与 `ui::welcome` 的 compact 档同量级）。
const BACKPLATE_MIN_WIDTH: u16 = 40;

// ===========================================================================
// 预检（三个前端共用）
// ===========================================================================

/// 预检结论（[`classify_preflight`] 的产物）。
pub(crate) enum Preflight {
    /// 配置可用：照常启动（含"网关早于 Setting API"的 404 兼容路径，见 design A2）。
    Ready,
    /// 网关降级（`valid=false`，或 503 `setup_mode`）：TUI 开 setup 循环、
    /// stdio / ACP 报 problems + `EX_CONFIG`。
    Unusable { problems: Vec<SettingProblem> },
    /// 预检请求本身失败（网络 / 401 / 403 / 5xx）：网关有毛病，不是配置有毛病。
    Failed(ApiClientError),
}

/// `GET /api/settings/status` + 分类 —— 三处（tui / stdio / acp）共用的唯一一次判定。
pub(crate) async fn preflight_config(http: &GatewayApiClient) -> Preflight {
    classify_preflight(http.settings_status().await)
}

/// 三种情况的分派（纯函数：单测直接喂六种输入，见 design D1 的表）。
fn classify_preflight(result: Result<SettingsStatusResponse, ApiClientError>) -> Preflight {
    match result {
        Ok(status) if status.valid => Preflight::Ready,
        Ok(status) => Preflight::Unusable {
            problems: status.problems,
        },
        // 预检是**增量能力**：拿不准"对面是不是带 Setting API 的网关"时，退化为今天的
        // 启动链（WS connect 的既有错误面会给出它自己的诊断）——绝不让预检把一个
        // 本来能用的网关挡在门外。
        //
        // 404 = 端点不存在：网关早于 Setting API。老网关在配置非法时**根本不会起进程**，
        // 所以"它活着"⇒ 配置可用（design A2；升级 CLI 后网关还没重启的窗口）。
        Err(e) if e.is_not_found() => Preflight::Ready,
        // 200 但 body 不是 status 的形状（代理 / 测试假网关 / 别的实现）：同上。
        // `reqwest` 把响应体解码失败包成 Transport（`is_decode()`），与"真的不可达"
        // 分得开；`Deserialize` 是同一件事在别处的形态。
        Err(ApiClientError::Transport(e)) if e.is_decode() => Preflight::Ready,
        Err(ApiClientError::Deserialize(_)) => Preflight::Ready,
        // 503 + error=setup_mode：网关确实在降级（status 在守门白名单里，正常不会走到这里，
        // 但万一路径被拦，进修复流程比报错对）。
        Err(e) if e.is_setup_mode() => Preflight::Unusable {
            problems: Vec::new(),
        },
        Err(e) => Preflight::Failed(e),
    }
}

/// `Preflight::Failed` 的可操作文案（三处共用；`{e}` 里带着后端的 detail 原文）。
pub(crate) fn preflight_failure_message(http_base: &str, error: &ApiClientError) -> String {
    match error {
        ApiClientError::Transport(_) | ApiClientError::Connection(_) => format!(
            "Failed to reach the gateway at {http_base}: {error}\n\
             Make sure the gateway is running: wing start"
        ),
        ApiClientError::Api { status, detail, .. } => {
            format!("gateway rejected the config preflight ({status}): {detail}")
        }
        other => format!("gateway config preflight failed: {other}"),
    }
}

/// stdio / ACP 的降级（D27）：配置不可用 → problems 到 **stderr** + `EX_CONFIG`。
///
/// `None` = 配置可用（或老网关兼容路径），调用方照常启动。
pub(crate) async fn preflight_or_report(
    http: &GatewayApiClient,
    http_base: &str,
) -> Option<ExitCode> {
    match preflight_config(http).await {
        Preflight::Ready => None,
        Preflight::Unusable { problems } => {
            // 出路提示里的路径取后端权威值（status 不带 config_path）；拿不到就省略。
            let config_path = http
                .settings_get()
                .await
                .ok()
                .map(|state| state.config_path);
            Some(report_unusable(&problems, config_path.as_deref()))
        }
        Preflight::Failed(e) => {
            eprintln!("wing error: {}", preflight_failure_message(http_base, &e));
            Some(ExitCode::FAILURE)
        }
    }
}

/// 打降级报告（**全模块唯一的一处 stderr 写**）并返回 `EX_CONFIG`。
fn report_unusable(problems: &[SettingProblem], config_path: Option<&str>) -> ExitCode {
    eprint!("{}", unusable_report(problems, config_path));
    ExitCode::from(EX_CONFIG)
}

/// 降级报告的文本（纯函数，单测喂输入断言；stdout 永远不参与）。
fn unusable_report(problems: &[SettingProblem], config_path: Option<&str>) -> String {
    let mut out = String::from("wing: 网关配置不可用，无法启动会话。\n");
    for problem in problems {
        out.push_str(&format!(
            "  · {}: {}\n",
            problem.path.as_deref().unwrap_or("<document>"),
            problem.message
        ));
    }
    out.push_str("修复方式：运行 `wing` 打开设置面板，或 `wing config doctor`");
    if let Some(path) = config_path {
        out.push_str(&format!(" / 编辑 {path}"));
    }
    out.push('\n');
    out
}

/// `Quit` 之后打印的一行出路（终端恢复后由调用方打印，§16.2）。
pub(crate) fn quit_note(config_path: Option<&str>) -> String {
    match config_path {
        Some(path) => {
            format!("已退出。配置仍不可用：wing config doctor 查看详情，或直接编辑 {path}")
        }
        None => "已退出。配置仍不可用：wing config doctor 查看详情".to_string(),
    }
}

// ===========================================================================
// 循环本体
// ===========================================================================

/// setup 循环的出口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupOutcome {
    /// 配置已就绪（保存让网关转入正常模式）：调用方继续今天的启动链。
    Ready,
    /// 用户退出：调用方打印 [`quit_note`] 后正常结束进程。
    Quit,
}

/// 生产入口：真网关 + crossterm 直读事件（`run_tui` 调它）。
pub async fn run_setup_tui<B: Backend>(
    terminal: &mut Terminal<B>,
    http: &GatewayApiClient,
    config: &mut AppConfig,
    endpoint: &GatewayEndpoint,
) -> Result<SetupOutcome> {
    run_setup_tui_with(terminal, http, config, endpoint, &mut TerminalEvents).await
}

/// 循环本体：三个依赖都可注入（**单测的全部入口**；生产入口只多接了一个事件源）。
async fn run_setup_tui_with<B, H, E>(
    terminal: &mut Terminal<B>,
    http: &H,
    config: &mut AppConfig,
    endpoint: &GatewayEndpoint,
    events: &mut E,
) -> Result<SetupOutcome>
where
    B: Backend,
    H: SetupBackend,
    E: SetupEvents,
{
    // 1. 数据：目录 + 文档并发拉（setup mode 下这两个读端点仍在守门白名单里）。
    let (schema, state) = tokio::join!(http.schema(), http.get());
    let schema = schema.map_err(|e| {
        anyhow!(
            "failed to load the settings catalog from {}: {e}",
            endpoint.http_base
        )
    })?;
    let state = state.map_err(|e| {
        anyhow!(
            "failed to load the settings document from {}: {e}",
            endpoint.http_base
        )
    })?;

    // 2. Interface 根（09 的本地读写；读失败 = 树里没有这个根，不拿默认值冒充）。
    let interface = http.read_interface();
    let mut interface_catalog: Option<SettingNode> =
        interface.as_ref().map(|source| source.catalog.clone());

    // 3. 面板：首屏 = 问题清单（D11；同一个组件服务 setup / 保存失败 / 随时查看）。
    let mut panel = SettingsPanel::new(&schema, state, interface, View::Problems);
    let mut schema = schema;

    let mut note = baseline_note(panel.problems());
    let mut ctrl_c_at: Option<Instant> = None;

    loop {
        draw_frame(
            terminal,
            &mut panel,
            &schema,
            interface_catalog.as_ref(),
            config,
            &note,
            true,
        )?;

        let key = match next_setup_input(terminal, events).await? {
            SetupInput::Key(key) => key,
            SetupInput::StreamClosed => return Ok(SetupOutcome::Quit),
            SetupInput::Handled => continue,
        };

        // Ctrl+C 永远归循环：双击退出（第一击只提示）。照 `App::handle_quit_key` 的口径。
        if crate::tui::is_quit_key(&key) {
            let now = Instant::now();
            if ctrl_c_at.is_some_and(|last| now.duration_since(last) < QUIT_DOUBLE_PRESS) {
                return Ok(SetupOutcome::Quit);
            }
            ctrl_c_at = Some(now);
            note = Note::hint("再按一次 Ctrl+C 退出");
            continue;
        }
        ctrl_c_at = None;

        if q_is_quit(&panel, &key) {
            return Ok(SetupOutcome::Quit);
        }

        // 任何其它按键把瞬态提示换回基线（保存回执 / Ctrl+C 提示都只挂到下一次按键）。
        note = baseline_note(panel.problems());

        match panel.handle_key(key) {
            SettingsAction::None => {}
            SettingsAction::PreviewInterface(doc) => {
                // 实时预览：背板的调色板每帧从 `config` 现算，下一帧就是新颜色（§15.3）。
                *config = crate::config::store::appconfig_from_doc(&doc);
            }
            SettingsAction::Save {
                gateway,
                base,
                interface,
                gateway_dirty,
                interface_dirty,
            } => {
                let (save_note, ready) = save_once(
                    http,
                    &mut panel,
                    gateway,
                    base,
                    interface,
                    gateway_dirty,
                    interface_dirty,
                )
                .await;
                note = save_note;
                if ready {
                    // 关闭 overlay（背板 + 就绪行），短暂停留让用户看见，然后交还启动链。
                    draw_frame(
                        terminal,
                        &mut panel,
                        &schema,
                        interface_catalog.as_ref(),
                        config,
                        &note,
                        false,
                    )?;
                    tokio::time::sleep(READY_DWELL).await;
                    return Ok(SetupOutcome::Ready);
                }
            }
            // setup 里没有可重启的会话/连接，网关此刻正服务着这条修复请求；忽略——
            // 但键位栏（`restart_required` 非空时 07 会挂出"Ctrl+R 立即重启"）会让人以为
            // 按下去有反应，所以给一句回执（N4）：重启要等进了正常 TUI 再做。
            SettingsAction::RestartGateway => {
                note = Note::hint("重启网关：进入 TUI 后按 Ctrl+R（setup 里不改网关进程）");
            }
            // 面板是 setup 唯一的 UI：关掉它 = 退出（design A5）。
            SettingsAction::Close { .. } => return Ok(SetupOutcome::Quit),
            SettingsAction::Reload => {
                let (new_schema, new_state) = tokio::join!(http.schema(), http.get());
                match (new_schema, new_state) {
                    (Ok(new_schema), Ok(new_state)) => {
                        panel.apply_snapshot(&new_schema, new_state);
                        schema = new_schema;
                    }
                    _ => note = Note::error("重新载入失败：网关不可达"),
                }
                if let Some(source) = http.read_interface() {
                    *config = crate::config::store::appconfig_from_doc(&source.doc);
                    panel.set_interface(source.clone());
                    interface_catalog = Some(source.catalog);
                }
            }
        }
    }
}

/// 循环关心的三种输入（[`next_setup_input`] 的产物）。
enum SetupInput {
    /// 该转给面板的按键。
    Key(KeyEvent),
    /// 事件源断了（真实环境只在 fatal 时）——循环当退出，不空转。
    StreamClosed,
    /// 已经在归一里处理掉了（Resize 已重画；鼠标 / tick 不消费）——继续循环。
    Handled,
}

/// 取下一个事件并归一。
///
/// `Resize` **不是**退出信号：用户在 setup 阶段调整窗口不该把向导连同未保存的编辑
/// 一起丢掉。布局由下一帧自动跟随（ratatui 的 `autoresize` 看后端尺寸），这里额外
/// 强制整屏重画一次——终端被 out-of-band 扰动过，diff 残影要靠清屏抹掉（与 `app`
/// 主循环 `Resize → needs_full_redraw` 同一条处置）。清屏失败只记日志：画面全在下一帧
/// 重画，不该被一次终端查询打死。
async fn next_setup_input<B: Backend, E: SetupEvents>(
    terminal: &mut Terminal<B>,
    events: &mut E,
) -> Result<SetupInput> {
    match events.next_event().await {
        Some(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => Ok(SetupInput::Key(key)),
        // 其余的 `Key`（Release / Repeat）、鼠标、粘贴、focus、tick：setup 不消费。
        Some(TermEvent::Resize(_, _)) => {
            if let Err(e) = terminal.clear() {
                tracing::warn!("terminal clear after resize failed: {e}");
            }
            Ok(SetupInput::Handled)
        }
        Some(_) => Ok(SetupInput::Handled),
        None => Ok(SetupInput::StreamClosed),
    }
}

/// `q` 算不算"退出"的意图：只在面板**没有任何开着的东西**时才算。
///
/// 编辑中的 `q` 是输入字符（必须给编辑器）；帮助 / 枚举选择 / 搜索 / 确认态里的 `q`
/// 同理，都归面板 —— 按了没反应也比把向导关掉强。
fn q_is_quit(panel: &SettingsPanel, key: &KeyEvent) -> bool {
    if key.code != KeyCode::Char('q') || !key.modifiers.is_empty() {
        return false;
    }
    let panel_busy = panel.edit_state().is_some()
        || panel.prompt().is_some()
        || panel.search_query().is_some()
        || panel.choices().is_some()
        || panel.view() == View::Help;
    !panel_busy
}

/// 一次 `s`：两半各自执行 + 合并回执；返回 `(状态行, 是否就绪)`。
///
/// 回执是**一行**（D5），但两半的结论谁都不能被谁盖掉：TUI 写入失败、Gateway 失败 /
/// 未就绪 / 后端 warnings（AD13）都进同一个 `parts`，按发生顺序拼起来。
async fn save_once<H: SetupBackend>(
    http: &H,
    panel: &mut SettingsPanel,
    gateway: Value,
    base: String,
    interface: Value,
    gateway_dirty: bool,
    interface_dirty: bool,
) -> (Note, bool) {
    /// 一句话里最多拼几条 —— 再多也读不过来（且会被面板宽度裁掉）。
    const MAX_PARTS: usize = 3;
    let mut parts: Vec<String> = Vec::new();

    // Interface 半边（本地原子写；一键保存两边，D10——两边各自成败）。
    let mut interface_ok: Option<bool> = None;
    if interface_dirty {
        match http.write_interface(&interface) {
            Ok(_) => interface_ok = Some(true),
            Err(e) => {
                interface_ok = Some(false);
                parts.push(format!("TUI 配置写入失败：{e}"));
            }
        }
    }

    // Gateway 半边。
    let mut gateway_saved: Option<SettingsSetResponse> = None;
    let mut ready = false;
    if gateway_dirty {
        let request = SettingsSetRequest {
            base: Some(base),
            document: gateway,
        };
        match http.set(&request).await {
            Ok(response) => {
                // 就绪判定：回执的 `setup_mode_exited` 是规范路径，`status.valid` 是兜底
                // （04 的语义：保存成功 ⇒ 网关就地转入正常模式）。
                ready = response.ok
                    && (response.setup_mode_exited
                        || matches!(http.status().await, Ok(status) if status.valid));
                if !response.ok {
                    parts.push(format!(
                        "设置未保存：{} 个问题，见问题清单",
                        response.problems.len()
                    ));
                } else if !ready {
                    // 文件写了但网关没就绪（A12）：绝不放行到"没有会话能力"的启动链。
                    parts.push("保存成功，但网关仍未就绪 — 修完问题再按 s".into());
                }
                // 后端的非致命说明（AD13："旧密钥没有保留"之类）必须让用户看见 ——
                // 回执就一行，但它是这一屏唯一的出口。
                for warning in &response.warnings {
                    parts.push(format!("⚠ {warning}"));
                }
                // 要重启才生效的键（AD1 的 Ctrl+R 在 setup 里是 no-op）：告诉用户去哪儿按。
                if !response.restart_required.is_empty() {
                    // 动作在前（一行会被 CJK 宽度裁掉尾巴）：先在 setup 里说清"去哪儿按"。
                    parts.push(format!(
                        "需重启网关（进 TUI 后 Ctrl+R）：{}",
                        response.restart_required.join("、")
                    ));
                }
                gateway_saved = Some(response);
            }
            Err(e) => parts.push(format!("保存失败：{e}")),
        }
    }

    if gateway_saved.is_some() || interface_ok.is_some() {
        // 一次合并两半：`ok=false` 时面板自己会切到问题清单视图（07 的 apply_save）。
        panel.apply_save(SaveOutcome {
            gateway: gateway_saved,
            interface_ok,
        });
    }

    let caveats = if parts.len() > MAX_PARTS {
        parts.truncate(MAX_PARTS);
        parts.join(" · ") + " …"
    } else {
        parts.join(" · ")
    };

    if ready {
        // 就绪 + 有要交代的事：把交代缀在就绪句后面（绝不静默吞掉 —— N4）。
        let text = if caveats.is_empty() {
            "✓ 配置就绪，正在启动…".to_string()
        } else {
            format!("✓ 配置就绪，正在启动…（{caveats}）")
        };
        return (Note::ok(text), true);
    }
    if caveats.is_empty() {
        return (Note::ok("设置已保存"), false);
    }
    (Note::error(caveats), false)
}

// ===========================================================================
// 可注入的依赖面（私有：单测喂假实现，生产走真网关 / 09 的本地读写）
// ===========================================================================

/// 设置循环的全部 I/O：网关的 4 个 Setting API 操作 + Interface 根的本地读写。
trait SetupBackend {
    async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError>;
    async fn get(&self) -> Result<SettingsGetResponse, ApiClientError>;
    async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError>;
    async fn set(&self, req: &SettingsSetRequest) -> Result<SettingsSetResponse, ApiClientError>;
    /// Interface 根的本地读取；读失败 → `None`（树里没有这个根）。
    fn read_interface(&self) -> Option<InterfaceSource>;
    /// Interface 根的本地写盘（09 的原子写 + `.bak`）。
    fn write_interface(&self, doc: &Value) -> Result<WriteOutcome, StoreError>;
}

impl SetupBackend for GatewayApiClient {
    async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError> {
        self.settings_schema().await
    }

    async fn get(&self) -> Result<SettingsGetResponse, ApiClientError> {
        self.settings_get().await
    }

    async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError> {
        self.settings_status().await
    }

    async fn set(&self, req: &SettingsSetRequest) -> Result<SettingsSetResponse, ApiClientError> {
        self.settings_set(req).await
    }

    fn read_interface(&self) -> Option<InterfaceSource> {
        crate::config::store::read_interface_doc()
            .ok()
            .map(|read| InterfaceSource {
                catalog: crate::config::catalog::interface_catalog(),
                doc: read.doc,
            })
    }

    fn write_interface(&self, doc: &Value) -> Result<WriteOutcome, StoreError> {
        crate::config::store::write_interface_doc(doc)
    }
}

/// 事件源（生产 = crossterm 直读；测试 = 脚本序列）。
trait SetupEvents {
    async fn next_event(&mut self) -> Option<TermEvent>;
}

/// 生产事件源：`spawn_blocking` 里 poll(100ms) + read，**没有常驻读者**——
/// 退出即静默，不会像长驻任务那样在 `run_app` 的读者起来之前偷走一个按键。
struct TerminalEvents;

impl SetupEvents for TerminalEvents {
    async fn next_event(&mut self) -> Option<TermEvent> {
        tokio::task::spawn_blocking(|| {
            loop {
                match crossterm::event::poll(Duration::from_millis(100)) {
                    Ok(true) => match crossterm::event::read() {
                        Ok(crossterm::event::Event::Key(key))
                            if key.kind == KeyEventKind::Press =>
                        {
                            return Some(TermEvent::Key(key));
                        }
                        Ok(crossterm::event::Event::Resize(width, height)) => {
                            return Some(TermEvent::Resize(width, height));
                        }
                        // 鼠标 / 粘贴 / focus：setup 不消费，继续等（不做鼠标，design Non-Goals）。
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::error!("crossterm read error: {e}");
                            return None;
                        }
                    },
                    Ok(false) => continue,
                    Err(e) => {
                        tracing::error!("crossterm poll error: {e}");
                        return None;
                    }
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
}

// ===========================================================================
// 画面
// ===========================================================================

/// 一帧：背板（海鸥 + wordmark + 状态行）+ 面板（`panel_open = false` 时只有背板）。
fn draw_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    panel: &mut SettingsPanel,
    schema: &SettingsSchemaResponse,
    interface_catalog: Option<&SettingNode>,
    config: &AppConfig,
    note: &Note,
    panel_open: bool,
) -> Result<()> {
    // 调色板每帧现算：Interface 根的实时预览因此一按键就可见（§15.3 的同一条路径）。
    let palette = ThemePalette::from_config(&config.colors);
    // `crate::tui::draw_frame`（而不是 `terminal.draw`）：向导是同一块终端上的第二个
    // 绘制面，它画出的设置值同样可能带上 VS16 表情符号（见 `crate::ui::emoji_width`）。
    crate::tui::draw_frame(terminal, |frame| {
        let (backplate, panel_area, note_area) = split_setup_areas(frame.area());
        if let Some(backplate) = backplate {
            draw_backplate(frame, backplate, &palette);
        }
        // 状态行**恒有**一行：它是唯一会说话的地方（保存回执 / 就绪 / Ctrl+C 提示），
        // 小终端里也不该整条消失。
        draw_note(frame, note_area, &palette, note);
        if panel_open {
            // 08 的契约：每帧同步可见行数，再 Clear + 整块 render（同 10 的落点）。
            panel.set_viewport_rows(tree_viewport_rows(panel, panel_area) as usize);
            frame.render_widget(Clear, panel_area);
            let catalogs = SettingsCatalogs::new(&schema.root, interface_catalog);
            frame.render_widget(SettingsOverlay::new(panel, catalogs, &palette), panel_area);
        } else {
            // 面板已关（就绪帧）：显式清掉那一块，否则 ratatui 的 diff 会留着上一帧的面板。
            frame.render_widget(Clear, panel_area);
        }
    })
    .map_err(|e| anyhow!("terminal draw failed: {e}"))?;
    Ok(())
}

/// 背板（上 14 行）、面板（其余）与状态行（恒有）的切分。
///
/// 状态行是**唯一**会说话的地方：保存回执、就绪、Ctrl+C 提示都在那里 —— 放得下时它是
/// 背板的最后一行，放不下时（窄 / 矮）就从面板手里拿一行，绝不被整条丢掉。背板（海鸥 +
/// wordmark）纯粹是门面，空间不够时先让它退场。
fn split_setup_areas(area: Rect) -> (Option<Rect>, Rect, Rect) {
    // 一行的状态行（高度 0 的终端里退化为 0 行）。
    let one_line = Rect {
        y: area.bottom().saturating_sub(1),
        height: area.height.min(1),
        ..area
    };
    if area.height >= BACKPLATE_MIN_HEIGHT && area.width >= BACKPLATE_MIN_WIDTH {
        let backplate = Rect {
            height: BACKPLATE_ROWS,
            ..area
        };
        let panel = Rect {
            y: area.y + BACKPLATE_ROWS,
            height: area.height - BACKPLATE_ROWS,
            ..area
        };
        // 背板里状态行是它的最后一行（`draw_backplate` 画海鸥与 wordmark，状态行照旧）。
        let note = Rect {
            y: area.y + BACKPLATE_ROWS - 1,
            height: 1,
            ..area
        };
        (Some(backplate), panel, note)
    } else {
        let panel = Rect {
            height: area.height.saturating_sub(one_line.height),
            ..area
        };
        (None, panel, one_line)
    }
}

/// 状态行：一句话 + 语气，居中压在给定的一行里。
fn draw_note(frame: &mut Frame, area: Rect, palette: &ThemePalette, note: &Note) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let line = Line::from(Span::styled(note.text.clone(), note.style(palette)));
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

/// 背板：海鸥 + wordmark 居中（状态行由 `draw_note` 单独画）。
fn draw_backplate(frame: &mut Frame, area: Rect, palette: &ThemePalette) {
    frame.render_widget(Clear, area);
    let accent = to_rgb(palette.accent);
    let light = is_light_theme(to_rgb(palette.text));
    let art_width = art::PERCHED_IDLE_COLS + ART_GAP + art::WORDMARK_COLS;

    if area.width as usize >= art_width {
        let x = area.x + (area.width - art_width as u16) / 2;
        draw_gull(
            frame,
            Rect::new(
                x,
                area.y,
                art::PERCHED_IDLE_COLS as u16,
                ART_TERM_ROWS as u16,
            ),
            accent,
        );
        draw_wordmark(
            frame,
            x + (art::PERCHED_IDLE_COLS + ART_GAP) as u16,
            area.y,
            accent,
            light,
        );
    } else if area.width as usize >= art::WORDMARK_COLS {
        let x = area.x + (area.width - art::WORDMARK_COLS as u16) / 2;
        draw_wordmark(frame, x, area.y, accent, light);
    }
}

/// 海鸥（站姿，定格）：与 `ui::welcome` 的待机帧同一份网格与留白，逐帧重画。
fn draw_gull(frame: &mut Frame, area: Rect, accent: sprite::Rgb) {
    let mut padded: Vec<&str> = vec![""; PERCHED_TOP_PAD];
    padded.extend_from_slice(art::PERCHED_IDLE);
    let lines = sprite::lines_padded(&padded, accent, 0, 0, ART_TERM_ROWS);
    frame.render_widget(Paragraph::new(lines), area);
}

/// wordmark（像素大字 `WING`，无扫光）：在背板里竖直居中。
fn draw_wordmark(frame: &mut Frame, x: u16, top: u16, accent: sprite::Rgb, light: bool) {
    let lines = wordmark::lines(None, accent, light);
    let y = top + (ART_TERM_ROWS as u16).saturating_sub(lines.len() as u16) / 2;
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(x, y, art::WORDMARK_COLS as u16, 3),
    );
}

/// 背板底行的状态行：一句话 + 语气。
struct Note {
    text: String,
    kind: NoteKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NoteKind {
    /// 常态说明（首屏主句）。
    Info,
    /// 低强度提示（Ctrl+C 第一击）。
    Hint,
    /// 成功（保存 / 就绪）。
    Ok,
    /// 失败或需要行动（问题计数 / 保存失败）。
    Error,
}

impl Note {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoteKind::Info,
        }
    }

    fn hint(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoteKind::Hint,
        }
    }

    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoteKind::Ok,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: NoteKind::Error,
        }
    }

    fn style(&self, palette: &ThemePalette) -> Style {
        match self.kind {
            NoteKind::Info => Style::default().fg(palette.text),
            NoteKind::Hint => Style::default().fg(palette.dim),
            NoteKind::Ok => Style::default()
                .fg(palette.success)
                .add_modifier(Modifier::BOLD),
            NoteKind::Error => Style::default().fg(palette.warning),
        }
    }
}

/// 首屏状态行（纯函数）：`reason` 不在 wire 上，用 problems 的形状分辨"首次运行"
/// 与"配置损坏"（design A1——两者只差一行措辞）。
fn baseline_note(problems: &[Problem]) -> Note {
    if problems.is_empty() {
        return Note::ok("配置就绪");
    }
    let first_run = problems.iter().all(|problem| {
        problem.kind == "empty_list"
            && matches!(problem.path.as_deref(), Some("providers") | Some("agents"))
    });
    if first_run {
        Note::info("首次运行 · 需要配置一个模型 provider")
    } else {
        Note::error(format!("配置不可用 · 需要修复 {} 个问题", problems.len()))
    }
}

#[cfg(test)]
mod tests {
    //! setup 循环与预检的测试（design.md 的测试策略表 ①–⑩）。
    //!
    //! 夹具自带（07 / 08 的 `test_support` 是各自包私有的）：一棵"gateway 下一个
    //! bool"的极小目录 + 一份 Interface 的 `colors.preset` 枚举；后端与事件源都是
    //! 可注入的假实现（12 在 `cmd/config.rs` 的同一套做法），**不碰磁盘**。

    use std::cell::Cell;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::ClearType;
    use ratatui::backend::TestBackend;
    use ratatui::backend::WindowSize;
    use ratatui::buffer::Buffer;
    use ratatui::buffer::Cell as BufferCell;
    use ratatui::layout::Position;
    use ratatui::layout::Rect;
    use ratatui::layout::Size;
    use ratatui::style::Color;
    use serde_json::json;
    use wing_api_client::models::ApplyScope;
    use wing_api_client::models::ErrorResponse;
    use wing_api_client::models::SecretState;
    use wing_api_client::models::SettingChoice;
    use wing_api_client::models::SettingKind;
    use wing_api_client::models::SettingsSetResponse;

    use super::*;
    use crate::config::ColorPreset;

    // ── 目录 / 文档夹具 ─────────────────────────────────────────

    fn node(key: &str, kind: SettingKind) -> SettingNode {
        SettingNode {
            key: key.into(),
            path: key.into(),
            title: key.into(),
            doc: format!("{key} 的说明"),
            notes: vec![],
            example: None,
            order: 0,
            kind,
            required: false,
            nullable: false,
            default: None,
            has_default: false,
            min: None,
            max: None,
            exclusive_min: false,
            exclusive_max: false,
            min_length: None,
            pattern: None,
            choices: vec![],
            min_items: None,
            max_items: None,
            secret: false,
            apply: ApplyScope::Hot,
            editable: true,
            deprecated: None,
            section: None,
            section_doc: None,
            children: vec![],
            element: None,
            variants: None,
            summary_fields: vec![],
            value_hint: None,
        }
    }

    fn with_paths(mut node: SettingNode, prefix: &str) -> SettingNode {
        node.path = if prefix.is_empty() {
            node.key.clone()
        } else {
            format!("{prefix}.{}", node.key)
        };
        let path = node.path.clone();
        let children = std::mem::take(&mut node.children);
        node.children = children
            .into_iter()
            .map(|child| with_paths(child, &path))
            .collect();
        node
    }

    fn object(key: &str, children: Vec<SettingNode>) -> SettingNode {
        let mut node = node(key, SettingKind::Object);
        node.children = children
            .into_iter()
            .map(|child| with_paths(child, key))
            .collect();
        node
    }

    fn bool_field(key: &str) -> SettingNode {
        node(key, SettingKind::Bool)
    }

    /// Gateway 根：`config.gateway.enabled`（bool，Enter 一按就脏）。
    fn gateway_catalog() -> SettingNode {
        let mut root = node("config", SettingKind::Object);
        root.path = "config".into();
        root.children = vec![with_paths(
            object("gateway", vec![bool_field("enabled")]),
            "",
        )];
        root
    }

    /// Interface 根：`interface.colors.preset`（enum，`→` 一按就预览）。
    fn interface_catalog_fixture() -> SettingNode {
        let mut preset = node("preset", SettingKind::Enum);
        preset.choices = vec![
            SettingChoice {
                value: "wing".into(),
                doc: Some("品牌配色".into()),
            },
            SettingChoice {
                value: "terminal".into(),
                doc: Some("跟随终端".into()),
            },
        ];
        preset.has_default = true;
        preset.default = Some(json!("wing"));
        // 一个可自由输入的自由文本字段（`accent`）：`q` 守卫的测试要一个真编辑器。
        let mut accent = node("accent", SettingKind::Str);
        accent.nullable = true;
        accent.example = Some("#f2c14e".into());
        let colors = object("colors", vec![preset, accent]);
        let mut root = node("interface", SettingKind::Object);
        root.path = "interface".into();
        root.children = vec![with_paths(colors, "")];
        root
    }

    fn problem(path: &str, kind: &str, message: &str) -> SettingProblem {
        SettingProblem {
            path: Some(path.into()),
            kind: kind.into(),
            message: message.into(),
            hint: None,
        }
    }

    fn schema() -> SettingsSchemaResponse {
        SettingsSchemaResponse {
            version: "0.0.0-test".into(),
            root: gateway_catalog(),
            config_path: "/home/u/.wing/core/config.yaml".into(),
        }
    }

    /// setup mode 的第一次读：模板刚落地，两个 `empty_list`（`providers` / `agents`）。
    fn first_run_state() -> SettingsGetResponse {
        SettingsGetResponse {
            values: json!({}),
            secrets: HashMap::<String, SecretState>::new(),
            fingerprint: "fp-1".into(),
            problems: vec![
                problem("providers", "empty_list", "providers list cannot be empty"),
                problem("agents", "empty_list", "agents list cannot be empty"),
            ],
            // 与真后端一致：04 的 get/status 里这个字段本期恒 false（design A1）。
            setup_mode: false,
            config_path: "/home/u/.wing/core/config.yaml".into(),
        }
    }

    fn interface_source(preset: &str) -> InterfaceSource {
        InterfaceSource {
            catalog: interface_catalog_fixture(),
            doc: json!({"colors": {"preset": preset}}),
        }
    }

    fn endpoint() -> GatewayEndpoint {
        GatewayEndpoint {
            ws_url: "ws://127.0.0.1:39881/ws".into(),
            http_base: "http://127.0.0.1:39881".into(),
            api_key: None,
        }
    }

    // ── 假后端 / 假事件源 ───────────────────────────────────────

    /// 假后端的 `set` 结果（`ApiClientError` 不可 Clone，用可克隆的替身）。
    #[derive(Clone)]
    enum FakeSet {
        Saved(SettingsSetResponse),
        Failed,
    }

    /// 假后端的脚本化响应 + 调用记录。
    struct FakeBackend {
        schema: SettingsSchemaResponse,
        state: SettingsGetResponse,
        status: SettingsStatusResponse,
        set: FakeSet,
        interface: Option<InterfaceSource>,
        /// 让 Interface 半边的原子写失败（N4 的两半独立性测试）。
        interface_write_fails: bool,
        sets: RefCell<Vec<SettingsSetRequest>>,
        written_interface: RefCell<Vec<Value>>,
    }

    impl FakeBackend {
        fn first_run() -> Self {
            Self {
                schema: schema(),
                state: first_run_state(),
                status: SettingsStatusResponse {
                    valid: false,
                    setup_mode: false,
                    problems: first_run_state().problems,
                    fingerprint: Some("fp-1".into()),
                },
                set: FakeSet::Saved(saved_response(true, "fp-2", true, vec![])),
                interface: None,
                interface_write_fails: false,
                sets: RefCell::new(Vec::new()),
                written_interface: RefCell::new(Vec::new()),
            }
        }

        fn sets(&self) -> Vec<SettingsSetRequest> {
            self.sets.borrow().clone()
        }

        fn written_interface(&self) -> Vec<Value> {
            self.written_interface.borrow().clone()
        }
    }

    impl SetupBackend for FakeBackend {
        async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError> {
            Ok(self.schema.clone())
        }

        async fn get(&self) -> Result<SettingsGetResponse, ApiClientError> {
            Ok(self.state.clone())
        }

        async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError> {
            Ok(self.status.clone())
        }

        async fn set(
            &self,
            req: &SettingsSetRequest,
        ) -> Result<SettingsSetResponse, ApiClientError> {
            self.sets.borrow_mut().push(req.clone());
            match &self.set {
                FakeSet::Saved(response) => Ok(response.clone()),
                FakeSet::Failed => Err(ApiClientError::Api {
                    status: 500,
                    detail: "保存服务炸了".into(),
                    body: None,
                }),
            }
        }

        fn read_interface(&self) -> Option<InterfaceSource> {
            self.interface.clone()
        }

        fn write_interface(&self, doc: &Value) -> Result<WriteOutcome, StoreError> {
            if self.interface_write_fails {
                return Err(StoreError::Write {
                    path: std::path::PathBuf::from("/home/u/.wing/tui/config.yaml"),
                    source: std::io::Error::other("磁盘满了"),
                });
            }
            self.written_interface.borrow_mut().push(doc.clone());
            Ok(WriteOutcome {
                path: std::path::PathBuf::from("/home/u/.wing/tui/config.yaml"),
                fingerprint: "fp-tui".into(),
                backup_path: None,
            })
        }
    }

    fn saved_response(
        ok: bool,
        fingerprint: &str,
        exited: bool,
        problems: Vec<SettingProblem>,
    ) -> SettingsSetResponse {
        SettingsSetResponse {
            warnings: Vec::new(),
            ok,
            fingerprint: fingerprint.into(),
            problems,
            changed: vec!["gateway.enabled".into()],
            restart_required: vec![],
            reload: None,
            setup_mode_exited: exited,
            backup_path: None,
        }
    }

    /// 脚本化事件源：按序吐帧，吐完 `None`（循环当"事件流断了"处理 ⇒ Quit）。
    ///
    /// 于是"脚本耗尽"能当退出用——**它对终帧的影响是零**：循环在取事件之前画帧，
    /// 所以最后一帧就是最后一个按键处理完的那一帧（断言"保存之后留下了什么"就靠它）。
    struct ScriptedEvents {
        queue: VecDeque<TermEvent>,
        /// `Some` = 这条脚本的终端是 [`ResizableBackend`]：吐出 `Resize(w, h)` 之前
        /// **先**把后端的自报尺寸换过去（真终端的顺序：终端先换尺寸，SIGWINCH 事件后到）。
        size: Option<Rc<Cell<(u16, u16)>>>,
    }

    impl SetupEvents for ScriptedEvents {
        async fn next_event(&mut self) -> Option<TermEvent> {
            let event = self.queue.pop_front();
            if let (Some(size), Some(TermEvent::Resize(width, height))) = (&self.size, &event) {
                size.set((*width, *height));
            }
            event
        }
    }

    fn key(code: KeyCode) -> TermEvent {
        TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_c() -> TermEvent {
        TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
    }

    fn text(value: &str) -> Vec<TermEvent> {
        value.chars().map(|c| key(KeyCode::Char(c))).collect()
    }

    fn script(events: impl IntoIterator<Item = TermEvent>) -> ScriptedEvents {
        ScriptedEvents {
            queue: events.into_iter().collect(),
            size: None,
        }
    }

    /// 走完一遍"搜索 → 编辑 → 保存"的按键路径（search 会跨根命中）。
    fn edit_and_save(needle: &str) -> Vec<TermEvent> {
        let mut events = vec![key(KeyCode::Char('/'))];
        events.extend(text(needle));
        events.extend([
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            key(KeyCode::Char('s')),
        ]);
        events
    }

    fn test_terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(120, 40)).expect("test terminal")
    }

    /// 测试用后端：在 `TestBackend` 外套一层"尺寸可被外部改"的壳。
    ///
    /// 真终端的 resize 是**后端自报新尺寸**、ratatui 的 `autoresize` 据此重排 ——
    /// `TestBackend` 建好之后尺寸写死，所以要复现"下一帧按新尺寸画"必须自己造这个壳。
    struct ResizableBackend {
        inner: RefCell<TestBackend>,
        size: Rc<Cell<(u16, u16)>>,
    }

    impl ResizableBackend {
        /// 返回后端与它的尺寸把手（测试改把手 = 真终端改窗口大小）。
        fn new(width: u16, height: u16) -> (Self, Rc<Cell<(u16, u16)>>) {
            let size = Rc::new(Cell::new((width, height)));
            let backend = Self {
                inner: RefCell::new(TestBackend::new(width, height)),
                size: Rc::clone(&size),
            };
            (backend, size)
        }

        /// 当前屏幕缓冲（壳里那一块，clone 出来看）。
        fn buffer(&self) -> Buffer {
            self.inner.borrow().buffer().clone()
        }
    }

    impl Backend for ResizableBackend {
        type Error = core::convert::Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a BufferCell)>,
        {
            self.inner.borrow_mut().draw(content)
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.borrow_mut().hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.borrow_mut().show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.inner.borrow_mut().get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.inner.borrow_mut().set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.borrow_mut().clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.inner.borrow_mut().clear_region(clear_type)
        }

        /// 自报尺寸跟着把手走；变了就把内层缓冲一起 resize（真终端这一层由内核做）。
        fn size(&self) -> Result<Size, Self::Error> {
            let (width, height) = self.size.get();
            let mut inner = self.inner.borrow_mut();
            let current = inner.buffer().area;
            if current.width != width || current.height != height {
                inner.resize(width, height);
            }
            Ok(Size::new(width, height))
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.inner.borrow_mut().window_size()
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            self.inner.borrow_mut().flush()
        }
    }

    /// 一屏文本（不含样式）。
    ///
    /// 双宽字符（CJK）在缓冲里占两格、第二格是 reset 出来的占位格：按**显示宽度**
    /// 跳格，否则会读出「每 个 汉 字 一 个 空 格」的假象（与 `ui/settings/tests.rs`
    /// 的 `row_text` 同一口径）。被 diff 跳过的占位格也正因此不会污染文本。
    fn buffer_text(buffer: &Buffer) -> String {
        (buffer.area.y..buffer.area.bottom())
            .map(|y| {
                let mut row = String::new();
                let mut x = buffer.area.x;
                while x < buffer.area.right() {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    let width = unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
                    x += width;
                }
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        buffer_text(terminal.backend().buffer())
    }

    async fn run(
        backend: &FakeBackend,
        config: &mut AppConfig,
        events: Vec<TermEvent>,
    ) -> (SetupOutcome, Terminal<TestBackend>) {
        let mut terminal = test_terminal();
        let mut events = script(events);
        let outcome = run_setup_tui_with(&mut terminal, backend, config, &endpoint(), &mut events)
            .await
            .expect("setup loop");
        (outcome, terminal)
    }

    // ── ① 首屏 = 问题清单 ───────────────────────────────────────

    #[tokio::test]
    async fn first_screen_is_the_problem_list() {
        let backend = FakeBackend::first_run();
        let mut config = AppConfig::default();
        // 空脚本：画完首帧后事件流关闭 ⇒ 循环当退出处理，首帧因此就是终帧。
        let (outcome, terminal) = run(&backend, &mut config, vec![]).await;

        assert!(matches!(outcome, SetupOutcome::Quit));
        let screen = screen(&terminal);
        assert!(screen.contains("待修复"), "{screen}");
        assert!(
            screen.contains("providers list cannot be empty"),
            "{screen}"
        );
        assert!(screen.contains("agents list cannot be empty"), "{screen}");
    }

    // ── ② 背板画出来了 ──────────────────────────────────────────

    #[tokio::test]
    async fn the_backplate_is_drawn() {
        let backend = FakeBackend::first_run();
        let mut config = AppConfig::default();
        let (_, terminal) = run(&backend, &mut config, vec![]).await;

        let screen = screen(&terminal);
        // 说明行（首屏措辞：problems 全是 empty_list ⇒ "首次运行"）。
        assert!(
            screen.contains("首次运行 · 需要配置一个模型 provider"),
            "{screen}"
        );

        let buffer = terminal.backend().buffer();
        let half_blocks = buffer
            .content
            .iter()
            .filter(|cell| matches!(cell.symbol(), "▀" | "▄"))
            .count();
        assert!(
            half_blocks > 100,
            "海鸥 + wordmark 的半格像素太少：{half_blocks}"
        );
        // 海鸥品牌色（羽白）只可能来自帧数据。
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.fg == Color::Rgb(247, 249, 252)),
            "背板上没有海鸥的羽白像素"
        );
    }

    // ── ③ 保存后 valid → Ready ─────────────────────────────────

    #[tokio::test]
    async fn a_successful_save_returns_ready() {
        let backend = FakeBackend::first_run();
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, edit_and_save("enabled")).await;

        assert!(matches!(outcome, SetupOutcome::Ready), "{outcome:?}");
        let screen = screen(&terminal);
        assert!(screen.contains("✓ 配置就绪，正在启动…"), "{screen}");
        // 就绪帧 = 面板已关（问题清单不在）。
        assert!(!screen.contains("待修复"), "{screen}");

        let sets = backend.sets();
        assert_eq!(sets.len(), 1, "只发一次保存");
        assert_eq!(sets[0].base.as_deref(), Some("fp-1"), "乐观并发基线");
        assert_eq!(
            sets[0].document,
            json!({"gateway": {"enabled": true}}),
            "脏的那一半（bool 翻转）原样发出"
        );
    }

    #[tokio::test]
    async fn ready_falls_back_to_status_when_the_receipt_does_not_say_exited() {
        // `setup_mode_exited=false` + `status.valid=true` ⇒ 同样就绪（兜底路径）。
        let mut backend = FakeBackend::first_run();
        backend.set = FakeSet::Saved(saved_response(true, "fp-2", false, vec![]));
        backend.status = SettingsStatusResponse {
            valid: true,
            setup_mode: false,
            problems: vec![],
            fingerprint: Some("fp-2".into()),
        };
        let mut config = AppConfig::default();
        let (outcome, _) = run(&backend, &mut config, edit_and_save("enabled")).await;
        assert!(matches!(outcome, SetupOutcome::Ready), "{outcome:?}");
    }

    // ── ④ 保存后仍 invalid → 留在循环 ───────────────────────────

    #[tokio::test]
    async fn a_rejected_save_stays_in_the_loop_with_fresh_problems() {
        let mut backend = FakeBackend::first_run();
        backend.set = FakeSet::Saved(saved_response(
            false,
            "fp-1",
            false,
            vec![problem(
                "gateway.enabled",
                "invalid_value",
                "enabled 还不能是 true",
            )],
        ));
        // 保存后脚本耗尽（事件流关闭 ⇒ 退出）：保存后的那一帧就是终帧。
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, edit_and_save("enabled")).await;

        // 没有就绪 ⇒ 留在循环（否则会是 Ready），且回执带来的新问题上了屏。
        assert!(matches!(outcome, SetupOutcome::Quit));
        let screen = screen(&terminal);
        assert!(screen.contains("enabled 还不能是 true"), "{screen}");
        assert!(screen.contains("待修复"), "{screen}");
        assert_eq!(backend.sets().len(), 1);
    }

    #[tokio::test]
    async fn a_save_that_cannot_ready_the_gateway_does_not_leave_the_loop() {
        // `ok=true` 但既不 exited 且 status 仍 invalid：绝不放行走正常启动链（A12）。
        let mut backend = FakeBackend::first_run();
        backend.set = FakeSet::Saved(saved_response(true, "fp-2", false, vec![]));
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, edit_and_save("enabled")).await;

        assert!(matches!(outcome, SetupOutcome::Quit));
        assert!(screen(&terminal).contains("保存成功，但网关仍未就绪"));
    }

    // ── ⑤ Ctrl+C 双击 → Quit ────────────────────────────────────

    #[tokio::test]
    async fn ctrl_c_double_press_quits_with_a_hint_on_the_first_press() {
        let backend = FakeBackend::first_run();
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, vec![ctrl_c(), ctrl_c()]).await;

        assert!(matches!(outcome, SetupOutcome::Quit));
        assert!(screen(&terminal).contains("再按一次 Ctrl+C 退出"));
    }

    #[tokio::test]
    async fn esc_through_the_ladder_quits() {
        // 问题清单上 Esc = 回树，树上的第二击 = 面板的 Close ⇒ setup 里等价于退出。
        let backend = FakeBackend::first_run();
        let mut config = AppConfig::default();
        let (outcome, _) = run(
            &backend,
            &mut config,
            vec![key(KeyCode::Esc), key(KeyCode::Esc)],
        )
        .await;
        assert!(matches!(outcome, SetupOutcome::Quit));
    }

    #[tokio::test]
    async fn q_quits() {
        let backend = FakeBackend::first_run();
        let mut config = AppConfig::default();
        let (outcome, _) = run(&backend, &mut config, vec![key(KeyCode::Char('q'))]).await;
        assert!(matches!(outcome, SetupOutcome::Quit));
    }

    /// N1：编辑中的 `q` 必须是**输入字符**，不是退出。
    ///
    /// 判据落在"保存请求里有没有 `q`"上：守卫若丢了，`q` 会当场把循环关掉，
    /// 后面的事件一个都消费不了、接口半边也永远不会写盘。
    #[tokio::test]
    async fn q_in_an_open_editor_is_typed_not_quit() {
        let mut backend = FakeBackend::first_run();
        backend.interface = Some(interface_source("wing"));
        let mut events = vec![key(KeyCode::Char('/'))];
        events.extend(text("accent"));
        // 搜索命中 → 回车开编辑器 → 敲 `q` → 回车提交。
        events.extend([
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            key(KeyCode::Char('q')),
            key(KeyCode::Enter),
            key(KeyCode::Char('s')),
        ]);
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, events).await;

        assert!(matches!(outcome, SetupOutcome::Quit), "脚本耗尽 ⇒ 退出");
        let written = backend.written_interface();
        assert_eq!(written.len(), 1, "接口半边写盘了：\n{}", screen(&terminal));
        assert_eq!(
            written[0]["colors"]["accent"], "q",
            "编辑器里的 `q` 应该是输入字符"
        );
    }

    /// N1：枚举选择项视图开着时 `q` 也不该关向导（它属"面板开着的东西"）。
    #[tokio::test]
    async fn q_with_choices_open_is_not_quit() {
        let mut backend = FakeBackend::first_run();
        backend.interface = Some(interface_source("wing"));
        let mut events = vec![key(KeyCode::Char('/'))];
        events.extend(text("preset"));
        // 命中 → 回车退出搜索 → 回车展开选择项 → `q`（不该退出）→ 下移一格选
        // `terminal` → 回车落值 → 保存。
        events.extend([
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            key(KeyCode::Char('q')),
            key(KeyCode::Down),
            key(KeyCode::Enter),
            key(KeyCode::Char('s')),
        ]);
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, events).await;

        assert!(matches!(outcome, SetupOutcome::Quit), "脚本耗尽 ⇒ 退出");
        let written = backend.written_interface();
        assert_eq!(written.len(), 1, "接口半边写盘了：\n{}", screen(&terminal));
        assert_eq!(written[0]["colors"]["preset"], "terminal");
    }

    /// N1：帮助页开着时 `q` 也不该关向导（`?` 打开 → `q` → Esc 返回 → 照常编辑保存）。
    #[tokio::test]
    async fn q_with_help_open_is_not_quit() {
        let mut backend = FakeBackend::first_run();
        backend.interface = Some(interface_source("wing"));
        let mut events = vec![key(KeyCode::Char('?'))];
        events.push(key(KeyCode::Char('q')));
        events.push(key(KeyCode::Esc));
        events.push(key(KeyCode::Char('/')));
        events.extend(text("accent"));
        events.extend([
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            key(KeyCode::Char('x')),
            key(KeyCode::Enter),
            key(KeyCode::Char('s')),
        ]);
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, events).await;

        assert!(matches!(outcome, SetupOutcome::Quit), "脚本耗尽 ⇒ 退出");
        let written = backend.written_interface();
        assert_eq!(
            written.len(),
            1,
            "帮助页之后照常干活：\n{}",
            screen(&terminal)
        );
        assert_eq!(written[0]["colors"]["accent"], "x");
    }

    /// N4：就绪的那次保存带着"TUI 配置没写进去"的交代 —— 不能被就绪句吞掉。
    #[tokio::test]
    async fn a_ready_save_keeps_the_interface_failure_in_the_receipt() {
        let mut backend = FakeBackend::first_run();
        backend.interface = Some(interface_source("wing"));
        backend.interface_write_fails = true;
        // 两边都弄脏：Interface 半边写盘会失败，Gateway 半边成功且就绪 —— 就绪句
        // 必须把失败带上（N4.2）。
        let mut events = vec![key(KeyCode::Char('/'))];
        events.extend(text("preset"));
        events.extend([key(KeyCode::Enter), key(KeyCode::Right)]);
        events.push(key(KeyCode::Char('/')));
        events.extend(text("enabled"));
        events.extend([key(KeyCode::Enter), key(KeyCode::Enter)]);
        events.push(key(KeyCode::Char('s')));
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, events).await;

        assert_eq!(outcome, SetupOutcome::Ready, "网关半边就绪 ⇒ 进正常 TUI");
        let screen = screen(&terminal);
        assert!(screen.contains("✓ 配置就绪"), "{screen}");
        assert!(
            screen.contains("TUI 配置写入失败"),
            "写入失败必须跟着就绪句一起露面：\n{screen}"
        );
    }

    /// N4 + AD13：后端的 warnings 与"需重启才生效"都进回执（一行里拼得下）。
    #[tokio::test]
    async fn a_save_receipt_carries_the_backend_warnings() {
        let mut backend = FakeBackend::first_run();
        let mut response = saved_response(true, "fp-2", true, vec![]);
        response.warnings = vec!["原配置文件无法解析，其中的密钥无法保留，请重新填写".into()];
        response.restart_required = vec!["gateway.port".into()];
        backend.set = FakeSet::Saved(response);
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, edit_and_save("enabled")).await;

        assert_eq!(outcome, SetupOutcome::Ready, "网关半边就绪 ⇒ 进正常 TUI");
        let screen = screen(&terminal);
        assert!(
            screen.contains("密钥无法保留"),
            "AD13 的警告要看得见：\n{screen}"
        );
        assert!(screen.contains("Ctrl+R"), "重启去向要说清楚：\n{screen}");
    }

    /// N4.1：两半**同时**失败时，两句话都不许被对方盖掉（一行里拼起来）。
    #[tokio::test]
    async fn a_double_failure_keeps_both_halves_in_the_receipt() {
        let mut backend = FakeBackend::first_run();
        backend.interface = Some(interface_source("wing"));
        backend.interface_write_fails = true;
        backend.set = FakeSet::Failed;
        // 先把 Interface 半边弄脏（enum 右切），再把 Gateway 半边弄脏（bool 切换）。
        let mut events = vec![key(KeyCode::Char('/'))];
        events.extend(text("preset"));
        events.extend([key(KeyCode::Enter), key(KeyCode::Right)]);
        events.push(key(KeyCode::Char('/')));
        events.extend(text("enabled"));
        events.extend([key(KeyCode::Enter), key(KeyCode::Enter)]);
        events.push(key(KeyCode::Char('s')));
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, events).await;

        assert!(matches!(outcome, SetupOutcome::Quit));
        let screen = screen(&terminal);
        assert!(screen.contains("TUI 配置写入失败"), "{screen}");
        assert!(screen.contains("保存失败"), "{screen}");
    }

    #[tokio::test]
    async fn a_transport_failure_reports_and_stays_in_the_loop() {
        let mut backend = FakeBackend::first_run();
        backend.set = FakeSet::Failed;
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, edit_and_save("enabled")).await;

        assert!(matches!(outcome, SetupOutcome::Quit), "传输失败不构成就绪");
        assert!(
            screen(&terminal).contains("保存失败"),
            "{}",
            screen(&terminal)
        );
    }

    // ── ⑥ Interface 根的实时预览 ────────────────────────────────

    #[tokio::test]
    async fn interface_preview_applies_during_setup() {
        let mut backend = FakeBackend::first_run();
        backend.interface = Some(interface_source("wing"));
        let mut events = vec![key(KeyCode::Char('/'))];
        events.extend(text("preset"));
        events.extend([key(KeyCode::Enter), key(KeyCode::Right), ctrl_c(), ctrl_c()]);
        let mut config = AppConfig::default();
        let (outcome, _) = run(&backend, &mut config, events).await;

        assert!(matches!(outcome, SetupOutcome::Quit));
        // enum 右切：`wing` → `terminal`，预览直接写进调用方的 config（§15.3 的同一路径）。
        assert_eq!(config.colors.preset, ColorPreset::Terminal);
    }

    #[tokio::test]
    async fn interface_only_save_writes_the_local_file_and_stays() {
        let mut backend = FakeBackend::first_run();
        backend.interface = Some(interface_source("wing"));
        let mut events = vec![key(KeyCode::Char('/'))];
        events.extend(text("preset"));
        events.extend([
            key(KeyCode::Enter),
            key(KeyCode::Right),
            key(KeyCode::Char('s')),
        ]);
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(&backend, &mut config, events).await;

        assert!(
            matches!(outcome, SetupOutcome::Quit),
            "Interface 半边不决定就绪"
        );
        assert!(backend.sets().is_empty(), "Gateway 半边不脏就不发 POST");
        assert!(screen(&terminal).contains("设置已保存"));
        let written = backend.written_interface();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0], json!({"colors": {"preset": "terminal"}}));
    }

    // ── ⑦ 预检的三种情况分派 ────────────────────────────────────

    #[test]
    fn preflight_dispatch_covers_the_three_cases() {
        // 配置可用。
        assert!(matches!(
            classify_preflight(Ok(SettingsStatusResponse {
                valid: true,
                setup_mode: false,
                problems: vec![],
                fingerprint: None,
            })),
            Preflight::Ready
        ));

        // 配置不可用（降级启动）。
        match classify_preflight(Ok(SettingsStatusResponse {
            valid: false,
            setup_mode: false,
            problems: vec![problem("providers", "empty_list", "空")],
            fingerprint: Some("fp-1".into()),
        })) {
            Preflight::Unusable { problems } => assert_eq!(problems.len(), 1),
            _ => panic!("valid=false 必须进修复流程"),
        }

        // 404 = 网关早于 Setting API（老网关配置非法时根本不会起进程）。
        assert!(matches!(
            classify_preflight(Err(ApiClientError::Api {
                status: 404,
                detail: "Not Found".into(),
                body: None,
            })),
            Preflight::Ready
        ));

        // 503 + error=setup_mode：网关确实在降级。
        assert!(matches!(
            classify_preflight(Err(ApiClientError::Api {
                status: 503,
                detail: "setup mode".into(),
                body: Some(ErrorResponse {
                    error: "setup_mode".into(),
                    detail: None,
                    session_id: None,
                    uuid: None,
                }),
            })),
            Preflight::Unusable { .. }
        ));

        // 200 但 body 不是 status 的形状（假网关 / 代理）⇒ 退回今天的启动链。
        assert!(matches!(
            classify_preflight(Err(ApiClientError::Deserialize(
                serde_json::from_str::<serde_json::Value>("not json").unwrap_err()
            ))),
            Preflight::Ready
        ));

        // 网关不可达。
        assert!(matches!(
            classify_preflight(Err(ApiClientError::Connection("connection refused".into()))),
            Preflight::Failed(_)
        ));

        // 别的 HTTP 错误（401 / 403 / 500）都是"网关有毛病"。
        for status in [401, 403, 500] {
            assert!(matches!(
                classify_preflight(Err(ApiClientError::Api {
                    status,
                    detail: "detail 原文".into(),
                    body: None,
                })),
                Preflight::Failed(_)
            ));
        }
    }

    #[test]
    fn preflight_failure_messages_are_actionable() {
        // 网关不可达：保留今天那句可操作提示。
        let unreachable = preflight_failure_message(
            "http://127.0.0.1:39881",
            &ApiClientError::Connection("connection refused".into()),
        );
        assert!(
            unreachable.contains("http://127.0.0.1:39881"),
            "{unreachable}"
        );
        assert!(
            unreachable.contains("Make sure the gateway is running: wing start"),
            "{unreachable}"
        );

        // 后端 HTTP 错误：detail 原文（不吞）。
        let rejected = preflight_failure_message(
            "http://127.0.0.1:39881",
            &ApiClientError::Api {
                status: 500,
                detail: "设置服务炸了".into(),
                body: None,
            },
        );
        assert!(rejected.contains("500"), "{rejected}");
        assert!(rejected.contains("设置服务炸了"), "{rejected}");
    }

    // ── ⑧ 降级报告（stdio / ACP）────────────────────────────────

    #[test]
    fn unusable_report_lists_problems_and_the_way_out() {
        let report = unusable_report(
            &[
                problem("providers", "empty_list", "providers list cannot be empty"),
                SettingProblem {
                    path: None,
                    kind: "parse_error".into(),
                    message: "无法解析".into(),
                    hint: None,
                },
            ],
            Some("/tmp/wing-11/core/config.yaml"),
        );
        assert!(report.contains("网关配置不可用"), "{report}");
        assert!(
            report.contains("  · providers: providers list cannot be empty"),
            "{report}"
        );
        assert!(report.contains("  · <document>: 无法解析"), "{report}");
        assert!(report.contains("wing config doctor"), "{report}");
        assert!(report.contains("/tmp/wing-11/core/config.yaml"), "{report}");

        // 拿不到路径就省略那半句，但"怎么修"仍在。
        let without = unusable_report(&[], None);
        assert!(!without.contains("编辑"), "{without}");
        assert!(without.contains("wing config doctor"), "{without}");
    }

    #[test]
    fn unusable_exit_code_is_ex_config() {
        assert_eq!(EX_CONFIG, 78, "sysexits.h 的 EX_CONFIG");
    }

    // ── ⑨⑩ 退出提示 / 首屏措辞 ─────────────────────────────────

    #[test]
    fn quit_note_has_both_shapes() {
        let with_path = quit_note(Some("/tmp/wing-11/core/config.yaml"));
        assert!(with_path.contains("已退出"), "{with_path}");
        assert!(with_path.contains("wing config doctor"), "{with_path}");
        assert!(
            with_path.contains("/tmp/wing-11/core/config.yaml"),
            "{with_path}"
        );

        let without = quit_note(None);
        assert!(without.contains("已退出"), "{without}");
        assert!(!without.contains("编辑"), "{without}");
    }

    #[test]
    fn baseline_note_speaks_first_run_and_broken_config() {
        assert_eq!(
            baseline_note(&[
                super::Problem {
                    root: crate::shared::panels::settings::Root::Gateway,
                    path: Some("providers".into()),
                    kind: "empty_list".into(),
                    message: "providers list cannot be empty".into(),
                    hint: None,
                },
                super::Problem {
                    root: crate::shared::panels::settings::Root::Gateway,
                    path: Some("agents".into()),
                    kind: "empty_list".into(),
                    message: "agents list cannot be empty".into(),
                    hint: None,
                },
            ])
            .text,
            "首次运行 · 需要配置一个模型 provider"
        );

        let broken = baseline_note(&[super::Problem {
            root: crate::shared::panels::settings::Root::Gateway,
            path: None,
            kind: "parse_error".into(),
            message: "无法解析".into(),
            hint: None,
        }]);
        assert!(broken.text.contains("配置不可用"), "{}", broken.text);
        assert!(broken.text.contains('1'), "{}", broken.text);
    }

    // ── 布局阶梯 ────────────────────────────────────────────────

    #[test]
    fn layout_ladder_drops_the_backplate_on_short_terminals() {
        let (backplate, panel, note) = split_setup_areas(Rect::new(0, 0, 120, 40));
        let backplate = backplate.expect("40 行放得下背板");
        assert_eq!(backplate.height, BACKPLATE_ROWS);
        assert_eq!(panel.y, BACKPLATE_ROWS);
        assert_eq!(panel.height, 40 - BACKPLATE_ROWS);
        // 状态行是背板的最后一行。
        assert_eq!(note, Rect::new(0, BACKPLATE_ROWS - 1, 120, 1));

        // 24 行终端：背板让位，面板拿掉一行留给状态行（**状态行不消失** —— N3）。
        let (backplate, panel, note) = split_setup_areas(Rect::new(0, 0, 80, 24));
        assert!(backplate.is_none());
        assert_eq!(note, Rect::new(0, 23, 80, 1));
        assert_eq!(panel, Rect::new(0, 0, 80, 23));

        // 窄也一样（宽度掉到背板阈值以下）：面板 + 状态行。
        let (backplate, panel, note) = split_setup_areas(Rect::new(0, 0, 30, 40));
        assert!(backplate.is_none());
        assert_eq!(note, Rect::new(0, 39, 30, 1));
        assert_eq!(panel.height, 39);

        // 退化到 1 行 / 0 行也不 panic。
        let (_, panel, note) = split_setup_areas(Rect::new(0, 0, 20, 1));
        assert_eq!((panel.height, note.height), (0, 1));
        let (_, panel, note) = split_setup_areas(Rect::new(0, 0, 20, 0));
        assert_eq!((panel.height, note.height), (0, 0));
    }

    /// S1：`Resize` 必须被当成"重排 + 重画"，不是退出 —— 用户在 setup 阶段拉窗口
    /// 不能把向导连同未保存的编辑一起丢掉。
    ///
    /// 三个断言：(a) 循环没退出（最后的 `s` 还被执行了、结果是 `Ready`）；
    /// (b) 下一帧按**新尺寸**画（80×24 < 背板阈值 ⇒ 背板消失，而状态行还在）；
    /// (c) 未保存的编辑还在（保存请求里带着 resize 之前敲进去的 `gateway.enabled = true`）。
    #[tokio::test]
    async fn a_resize_event_repaints_at_the_new_size_and_keeps_the_edit() {
        let backend = FakeBackend::first_run();
        let mut config = AppConfig::default();
        let (mut terminal, size) = {
            let (backend, size) = ResizableBackend::new(120, 40);
            (Terminal::new(backend).expect("test terminal"), size)
        };

        // 编辑（未保存）：搜到 `enabled` 再回车切换成 true。
        let mut events = vec![key(KeyCode::Char('/'))];
        events.extend(text("enabled"));
        events.extend([key(KeyCode::Enter), key(KeyCode::Enter)]);
        // 拉窗口到 80×24（真终端顺序：后端先自报新尺寸，事件后到），然后保存。
        events.push(TermEvent::Resize(80, 24));
        events.push(key(KeyCode::Char('s')));

        let mut events = ScriptedEvents {
            queue: events.into_iter().collect(),
            size: Some(Rc::clone(&size)),
        };
        let outcome = run_setup_tui_with(
            &mut terminal,
            &backend,
            &mut config,
            &endpoint(),
            &mut events,
        )
        .await
        .expect("setup loop");

        // (a) 没退出：`s` 被处理了，而且真的就绪（Resize 若被当成 Quit，后面的事件一个都不会消费）。
        assert_eq!(outcome, SetupOutcome::Ready);
        // (c) 编辑活着：resize 之前切的那一刀出现在了保存请求里。
        assert_eq!(
            backend.sets()[0].document,
            json!({"gateway": {"enabled": true}})
        );

        // (b) 下一帧按新尺寸画：80×24 ⇒ 背板（海鸥 / wordmark）让位，状态行还在。
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area, Rect::new(0, 0, 80, 24), "缓冲跟着新尺寸");
        let screen = buffer_text(&buffer);
        assert!(
            !screen.contains("首次运行"),
            "24 行里不该还有背板：\n{screen}"
        );
        assert!(screen.contains("✓ 配置就绪"), "状态行要看得见：\n{screen}");
    }

    /// S1 的最小复现（审查者的脚本）：`[Resize, ctrl_c]` —— Resize 之后按键仍被处理，
    /// 所以第一击 Ctrl+C 的提示必须出现在终帧上。
    #[tokio::test]
    async fn a_resize_event_does_not_swallow_the_next_key() {
        let backend = FakeBackend::first_run();
        let mut config = AppConfig::default();
        let (outcome, terminal) = run(
            &backend,
            &mut config,
            vec![TermEvent::Resize(100, 30), ctrl_c()],
        )
        .await;

        assert!(matches!(outcome, SetupOutcome::Quit));
        let screen = screen(&terminal);
        assert!(screen.contains("再按一次 Ctrl+C 退出"), "{screen}");
    }
}
