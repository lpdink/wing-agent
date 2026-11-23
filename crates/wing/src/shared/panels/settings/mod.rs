//! SettingsPanel owns settings navigation, draft editing and save intents.

mod doc;
mod edit;
mod groups;
mod list;
mod model;
mod problems;
mod search;
mod tree;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

use std::collections::HashSet;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use serde_json::Value;
use serde_json::json;
use wing_api_client::models::ApplyScope;
use wing_api_client::models::SettingGroup;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;
use wing_api_client::models::SettingProblem;
use wing_api_client::models::SettingsGetResponse;
use wing_api_client::models::SettingsSchemaResponse;
use wing_api_client::models::SettingsSetResponse;

use crate::shared::doc_edit;

pub use doc::Root;
pub use doc::SettingsDoc;
pub use edit::Constraints;
pub use edit::EditState;
pub use edit::ScalarKind;
pub use groups::AnchorView;
pub use groups::GroupAnchor;
pub use problems::Problem;
pub use search::SearchFilter;
pub use tree::Row;
pub use tree::RowAction;
pub use tree::RowMarkers;
pub use tree::ValueText;
pub use tree::display_value;
pub use tree::flatten;

use super::PageKind;
use super::SelectionPanel;
use super::wrap_index;

use edit::EditEvent;
use search::SearchState;

/// The content of the settings list. Enum choices are nested rows in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Categories,
    Tree,
    Problems,
    Help,
}

/// Interface 根的注入内容：catalog + 分组 + 稀疏文档。
///
/// The Interface catalog and groups come from `config/catalog.rs`. An empty
/// group list falls back to the catalog sections.
#[derive(Debug, Clone)]
pub struct InterfaceSource {
    pub catalog: SettingNode,
    pub groups: Vec<SettingGroup>,
    pub doc: Value,
}

/// 一次保存的回执（App 在两边各自完成之后回灌）。
#[derive(Debug, Clone, PartialEq)]
pub struct SaveOutcome {
    /// Gateway 侧结果；`None` = 本次没有 gateway 改动（没发请求）。
    pub gateway: Option<SettingsSetResponse>,
    /// Interface 本地写盘结果；`None` = 本次没有 interface 改动（未尝试）。
    pub interface_ok: Option<bool>,
}

/// 模态提示的类别（08 据此选渲染形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// 纯确认：`Enter` 确认 / `Esc` 取消（删除、清空、放弃改动）。
    Confirm,
    /// 形态选择（union 列表新增）：`↑↓` + `Enter`。
    Variants,
}

/// 模态提示的只读投影（渲染层用；面板不暴露内部状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptView {
    pub kind: PromptKind,
    pub title: String,
    /// 后果 / 说明（可为空）。
    pub lines: Vec<String>,
    /// 可选项（空 = 纯确认）。
    pub options: Vec<String>,
    pub cursor: usize,
}

/// 面板对外产出的动作（10 执行）。
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsAction {
    /// 键被消费，无事发生（浏览 / 展开折叠 / 进搜索……）。
    None,
    /// Interface 根的任何提交：整份候选稀疏文档（`appconfig_from_doc` 直接吃它）。
    PreviewInterface(Value),
    /// `s`：两边一起保存。`gateway_dirty` / `interface_dirty` 告诉 App 哪边需要真的动作
    /// （跳过空 POST / 无谓的文件重写）。
    Save {
        gateway: Value,
        base: String,
        interface: Value,
        gateway_dirty: bool,
        interface_dirty: bool,
    },
    /// `Ctrl+R`：立即重启网关（`turn.working` 时是否拒绝由 App 判定）。
    RestartGateway,
    /// `Esc`：关闭面板；`discard: true` = 用户确认放弃未保存改动（App 回滚 Interface 预览）。
    Close { discard: bool },
    /// `R`：丢弃本地改动并重拉 `get`（App 拉完调用 [`SettingsPanel::apply_snapshot`]）。
    Reload,
}

/// 展开中的 enum 选择项（内联子行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceState {
    pub root: Root,
    /// enum 节点路径（选择项行自己的路径是合成的 `<enum>=<value>`）。
    pub path: String,
    /// 内项光标：`< choices.len()` = 第几个 choice，`== choices.len()` = `(unset)` 行。
    pub cursor: usize,
}

/// 待确认的模态提示（私有；对外只经 [`SettingsPanel::prompt`]）。
#[derive(Debug, Clone)]
enum Pending {
    DeleteItem {
        root: Root,
        list_path: String,
        index: usize,
        label: String,
    },
    ClearScalar {
        root: Root,
        path: String,
        label: String,
    },
    Variants {
        root: Root,
        list_path: String,
        cursor: usize,
    },
    Discard {
        intent: DiscardIntent,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscardIntent {
    Close,
    Reload,
    Restart,
}

/// 设置面板状态机。
pub struct SettingsPanel {
    gateway_catalog: SettingNode,
    interface_catalog: Option<SettingNode>,
    /// 后端 schema 的分组声明（Gateway 根的锚点来源；老网关 = 空表 → 按 section 推导）。
    gateway_groups: Vec<SettingGroup>,
    /// Interface 根的分组声明（`InterfaceSource.groups`；没注入 Interface 根时为空）。
    interface_groups: Vec<SettingGroup>,
    /// 分类列表（两份声明拼成；顺序即界面顺序）。
    anchors: Vec<GroupAnchor>,
    /// 分类光标（`anchors` 的下标）。
    group_cursor: usize,
    doc: SettingsDoc,
    model_edits: model::ModelEdits,
    /// 后端带回的问题（`get` / `set` 的 `problems`），与本地问题合并。
    backend_problems: Vec<SettingProblem>,
    problems: Vec<Problem>,
    expanded: HashSet<(Root, String)>,
    choices: Option<ChoiceState>,
    search: Option<SearchState>,
    filter: Option<SearchFilter>,
    edit: Option<EditState>,
    pending: Option<Pending>,
    rows: Vec<Row>,
    cursor: usize,
    view: View,
    help_back: View,
    help_offset: usize,
    help_last_offset: usize,
    problems_back: View,
    search_back: View,
    problem_cursor: usize,
    config_path: String,
    setup_mode: bool,
    stale: bool,
    restart_required: Vec<String>,
    viewport_rows: usize,
}

impl SettingsPanel {
    /// 打开面板：`schema` + `state` 是 `GET schema` / `GET get` 的一次快照，
    /// `interface` 由 09/10 提供（`None` = 树里没有 Interface 根），
    /// `initial_view` 让同一个组件服务三个场景（setup 首屏传 `View::Problems`）。
    pub fn new(
        schema: &SettingsSchemaResponse,
        state: SettingsGetResponse,
        interface: Option<InterfaceSource>,
        initial_view: View,
    ) -> Self {
        let interface_catalog = interface.as_ref().map(|source| source.catalog.clone());
        let interface_groups = interface
            .as_ref()
            .map(|source| source.groups.clone())
            .unwrap_or_default();
        let interface_doc = interface.map_or_else(|| json!({}), |source| source.doc);
        let doc = SettingsDoc::new(
            state.values,
            interface_doc,
            state.secrets,
            state.fingerprint,
        );
        let anchors = groups::build_anchors(
            &schema.root,
            &schema.groups,
            interface_catalog
                .as_ref()
                .map(|catalog| (catalog, interface_groups.as_slice())),
        );
        let mut panel = Self {
            gateway_catalog: schema.root.clone(),
            gateway_groups: schema.groups.clone(),
            interface_groups,
            interface_catalog,
            anchors,
            group_cursor: 0,
            doc,
            model_edits: model::ModelEdits::default(),
            backend_problems: state.problems,
            problems: Vec::new(),
            expanded: HashSet::new(),
            choices: None,
            search: None,
            filter: None,
            edit: None,
            pending: None,
            rows: Vec::new(),
            cursor: 0,
            view: initial_view,
            help_back: initial_view,
            help_offset: 0,
            help_last_offset: usize::MAX,
            problems_back: View::Categories,
            search_back: initial_view,
            problem_cursor: 0,
            config_path: schema.config_path.clone(),
            setup_mode: state.setup_mode,
            stale: false,
            restart_required: Vec::new(),
            viewport_rows: 10,
        };
        // 进入第 0 组：展开它的顶层成员，进入分类时就有内容（不用逐个 Enter）。
        panel.enter_group(0);
        panel
    }

    // ── 业务分组 ───────────────────────────────────────

    /// 当前选中的分组（没有锚点时 `None`：空目录 / 老网关的空 schema）。
    pub fn group(&self) -> Option<&GroupAnchor> {
        self.anchors.get(self.group_cursor)
    }

    /// 分类列表的全部锚点（渲染顺序 = 声明顺序）。
    pub fn anchors(&self) -> &[GroupAnchor] {
        &self.anchors
    }

    /// 分类光标。
    pub fn group_cursor(&self) -> usize {
        self.group_cursor
    }

    /// 分类列表的可见窗口（锚点数超过高度时滚动；kernel 的同一份数学）。
    pub fn anchors_visible_range(&self, visible_rows: usize) -> std::ops::Range<usize> {
        super::window_range(self.group_cursor, self.anchors.len(), visible_rows)
    }

    /// 分类列表的**整表**只读投影（含徽标：脏 / 问题数 / 搜索命中数）。
    ///
    /// 渲染路径不用它：面板每帧只画可见的几行，走 [`Self::anchor_view`]。整表投影是给
    /// 「一次要看全部锚点」的调用方留的（今天只有测试），因为它要为每个组扫一遍问题清单
    /// 与命中集。
    pub fn anchor_views(&self) -> Vec<AnchorView> {
        (0..self.anchors.len())
            .filter_map(|index| self.anchor_view(index))
            .collect()
    }

    /// 单个锚点的只读投影（`index` 越界 → `None`）。
    ///
    /// 渲染层只画可见的那几行，逐行问比整表投影便宜得多：一次投影要为**每个**组扫一遍
    /// 问题清单与整个搜索命中集，而 `anchor_views` 只有整表消费者（测试与外部查询）才要。
    pub fn anchor_view(&self, index: usize) -> Option<AnchorView> {
        let anchor = self.anchors.get(index)?;
        Some(AnchorView {
            title: anchor.title.clone(),
            selected: index == self.group_cursor,
            dirty: self.group_dirty(anchor),
            problems: self.group_problems(anchor),
            hits: self.filter.as_ref().map(|f| self.group_hits(anchor, f)),
        })
    }

    /// 组内有未保存的改动吗（任一成员的子树下有脏路径）。
    fn group_dirty(&self, anchor: &GroupAnchor) -> bool {
        anchor
            .members
            .iter()
            .any(|member| self.doc.has_dirty_below(anchor.root, member))
    }

    /// 组内的问题条数（文档级问题 `path == null` 不归任何组：标题栏的总数仍然算它）。
    fn group_problems(&self, anchor: &GroupAnchor) -> usize {
        self.problems
            .iter()
            .filter(|problem| self.problem_in_group(anchor, problem))
            .count()
    }

    fn problem_in_group(&self, anchor: &GroupAnchor, problem: &Problem) -> bool {
        if problem.root != anchor.root {
            return false;
        }
        let Some(path) = problem.path.as_deref() else {
            return false;
        };
        let head = path_head(path);
        anchor.members.iter().any(|member| member == head)
    }

    /// 搜索态下组内的命中数（左栏徽标 + 「跳到第一个有命中的组」都靠它）。
    fn group_hits(&self, anchor: &GroupAnchor, filter: &SearchFilter) -> usize {
        filter.hits_under(anchor.root, &anchor.members)
    }

    /// 选中一个分组：展开它的顶层成员、设置项回到首行、重算行。
    fn enter_group(&mut self, index: usize) {
        if self.anchors.is_empty() {
            // 没有锚点（空目录 / 读不出 schema）：设置项列表为空，但**问题清单必须照算**
            // —— setup 首屏正是"目录还没长出来、问题一大堆"的那个场景。
            self.group_cursor = 0;
            self.rebuild(None);
            return;
        }
        let index = index.min(self.anchors.len() - 1);
        let changed = index != self.group_cursor;
        self.group_cursor = index;
        if changed {
            self.cursor = 0;
            self.choices = None;
        }
        let anchor = self.anchors[index].clone();
        for member in &anchor.members {
            self.expanded.insert((anchor.root, member.clone()));
        }
        self.rebuild(None);
    }

    /// 分类列表 `↑↓`：钳制移动（不绕回）。
    fn move_group(&mut self, delta: isize) {
        if self.anchors.is_empty() {
            return;
        }
        let last = self.anchors.len() - 1;
        let next = (self.group_cursor as isize + delta).clamp(0, last as isize) as usize;
        if next != self.group_cursor {
            self.group_cursor = next;
        }
    }

    /// 某个路径所属的分组下标（问题清单跳转 / 搜索自动定位用）。
    fn group_index_for(&self, root: Root, path: &str) -> Option<usize> {
        let head = path_head(path);
        self.anchors.iter().position(|anchor| {
            anchor.root == root && anchor.members.iter().any(|member| member == head)
        })
    }

    /// 锚点表重算后把光标尽量留在同一组（按 id；找不到就钳制），
    /// 并尽量把设置项光标锚回原来那一行（`R` 重载 / 注入 Interface 根之后不该被弹回首行）。
    fn restore_group(&mut self, previous: (Root, &str), row_anchor: Option<(Root, String)>) {
        let index = self
            .anchors
            .iter()
            .position(|anchor| anchor.is(previous.0, previous.1))
            .unwrap_or_else(|| self.group_cursor.min(self.anchors.len().saturating_sub(1)));
        self.enter_group(index);
        if let Some((root, path)) = row_anchor
            && let Some(found) = self.row_index(root, &path)
        {
            self.cursor = found;
        }
    }

    /// 注入 / 刷新 Interface 根（10 重读 `~/.wing/tui/config.yaml` 后调用）。
    ///
    /// 锚点表随之重算（Interface 锚点出现 / 消失），光标尽量留在同一组。
    pub fn set_interface(&mut self, source: InterfaceSource) {
        let row_anchor = self.row_anchor();
        // 身份是 `(root, id)`；id 克隆一份，避免跨着后面的赋值借用 self。
        let group_id = self.group().map(|anchor| (anchor.root, anchor.id.clone()));
        self.interface_catalog = Some(source.catalog);
        self.interface_groups = source.groups;
        self.doc.set_interface_doc(source.doc);
        self.rebuild_anchors();
        self.prune_expanded();
        self.recompute_filter();
        match group_id {
            Some((root, id)) => self.restore_group((root, &id), row_anchor),
            None => self.rebuild(row_anchor),
        }
    }

    /// 重算锚点表（schema 换了 / Interface 根注入或撤走）。
    fn rebuild_anchors(&mut self) {
        let anchors = groups::build_anchors(
            &self.gateway_catalog,
            &self.gateway_groups,
            self.interface_catalog
                .as_ref()
                .map(|catalog| (catalog, self.interface_groups.as_slice())),
        );
        self.anchors = anchors;
        if self.group_cursor >= self.anchors.len() {
            self.group_cursor = self.anchors.len().saturating_sub(1);
        }
    }

    /// `R` 重载的落地：整份换掉 Gateway 的目录 / 文档 / 密文表 / 指纹 / 后端问题，
    /// 丢弃本地改动（编辑 / 提示 / 选择项一并收起）。10 在重拉 `schema`+`get` 之后调用。
    pub fn apply_snapshot(&mut self, schema: &SettingsSchemaResponse, state: SettingsGetResponse) {
        let row_anchor = self.row_anchor();
        // 身份是 `(root, id)`；id 克隆一份，避免跨着后面的赋值借用 self。
        let group_id = self.group().map(|anchor| (anchor.root, anchor.id.clone()));
        self.gateway_catalog = schema.root.clone();
        self.gateway_groups = schema.groups.clone();
        self.config_path = schema.config_path.clone();
        self.doc
            .reload_gateway(state.values, state.secrets, state.fingerprint);
        self.model_edits.clear();
        self.backend_problems = state.problems;
        self.setup_mode = state.setup_mode;
        self.stale = false;
        self.restart_required.clear();
        self.edit = None;
        self.pending = None;
        self.choices = None;
        self.rebuild_anchors();
        self.prune_expanded();
        self.recompute_filter();
        match group_id {
            Some((root, id)) => self.restore_group((root, &id), row_anchor),
            None => self.rebuild(row_anchor),
        }
    }

    /// 保存回执落地：成功 → 清该根脏标记并把当前文档记为基线；`ok=false` → 换 problems、
    /// 切到问题清单视图（保存失败后的入口），脏标记保留。
    pub fn apply_save(&mut self, outcome: SaveOutcome) {
        if let Some(response) = outcome.gateway {
            self.doc.set_fingerprint(response.fingerprint.clone());
            self.backend_problems = response.problems.clone();
            self.restart_required = response.restart_required.clone();
            if response.ok {
                self.doc.mark_baseline(Root::Gateway);
                self.model_edits.clear();
                self.stale = false;
                // 保存让网关从降级转入正常模式：标题栏的 `setup mode` 标记跟着熄掉
                // （否则要等 10/11 重建面板）。
                if response.setup_mode_exited {
                    self.setup_mode = false;
                }
            } else {
                self.open_problems();
                self.problem_cursor = 0;
            }
        }
        if outcome.interface_ok == Some(true) {
            self.doc.mark_baseline(Root::Interface);
        }
        let anchor = self.row_anchor();
        self.rebuild(anchor);
    }

    /// 收到 `settings_changed` 事件 / 409 冲突：指纹与本地不同 → 顶部横幅
    /// （“配置已被其它客户端修改，按 R 重新载入”）。**不自动覆盖**本地改动。
    pub fn on_settings_changed(&mut self, fingerprint: &str) {
        self.stale = fingerprint != self.doc.fingerprint();
    }

    /// 一个按键 → 一个对外意图。
    pub fn handle_key(&mut self, key: KeyEvent) -> SettingsAction {
        if self.edit.is_some() {
            return self.handle_edit_key(key);
        }
        if self.pending.is_some() {
            return self.handle_pending_key(key);
        }
        if self.view == View::Help {
            return self.handle_help_key(key);
        }
        if self.search.is_some() {
            return self.handle_search_key(key);
        }
        match self.view {
            View::Categories | View::Tree => self.handle_tree_key(key),
            View::Problems => self.handle_problems_key(key),
            View::Help => SettingsAction::None,
        }
    }

    // ── 只读访问器（渲染层与判态用） ──────────────────────────

    /// The current group’s settings or the combined search results.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// 右栏的光标行下标（在 [`SettingsPanel::rows`] 里，即当前分组的成员子树）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Center the visible window on the active row or the active enum choice.
    pub fn visible_range(&self, visible_rows: usize) -> std::ops::Range<usize> {
        let mut cursor = self.cursor;
        if self.cursor_on_choice_owner() {
            let choice = self
                .choices
                .as_ref()
                .expect("owner of active choices")
                .cursor;
            if self
                .rows
                .get(cursor + choice + 1)
                .is_some_and(|row| matches!(row.action, RowAction::Choose { selected: true, .. }))
            {
                cursor += choice + 1;
            }
        }
        super::window_range(cursor, self.rows.len(), visible_rows)
    }

    /// Set the page step from the actual list viewport.
    pub fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(1);
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn edit_state(&self) -> Option<&EditState> {
        self.edit.as_ref()
    }

    /// 展开中的 enum 选择项（`None` = 没有）。
    pub fn choices(&self) -> Option<&ChoiceState> {
        self.choices.as_ref()
    }

    /// 搜索激活时的原始 query（空串也是激活态）。
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(SearchState::query)
    }

    /// 当前命中数（标题栏）。
    pub fn search_hits(&self) -> usize {
        self.filter.as_ref().map_or(0, SearchFilter::hits)
    }

    pub fn prompt(&self) -> Option<PromptView> {
        self.prompt_view()
    }

    /// 已合并（本地 + 后端）并按严重度排序的问题表。
    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    /// 问题清单视图的光标。
    pub fn problem_cursor(&self) -> usize {
        self.problem_cursor
    }

    /// 两个根的脏路径总数（标题栏 `N unsaved`）。
    pub fn dirty_count(&self) -> usize {
        self.doc.dirty_count()
    }

    pub fn is_dirty(&self, root: Root, path: &str) -> bool {
        self.doc.is_dirty(root, path)
    }

    pub fn fingerprint(&self) -> &str {
        self.doc.fingerprint()
    }

    /// config.yaml 的绝对路径（标题栏）。
    pub fn config_path(&self) -> &str {
        &self.config_path
    }

    pub fn setup_mode(&self) -> bool {
        self.setup_mode
    }

    /// 有其它客户端改过配置（指纹对不上）→ 08 画顶部横幅。
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// 上次保存回执里的 `restart_required`（08 画「按 Ctrl+R 立即重启」）。
    pub fn restart_required(&self) -> &[String] {
        &self.restart_required
    }

    pub(crate) fn interface_catalog(&self) -> Option<&SettingNode> {
        self.interface_catalog.as_ref()
    }

    pub fn has_interface(&self) -> bool {
        self.interface_catalog.is_some()
    }

    pub fn footer_hint(&self) -> String {
        if self.edit.is_some() {
            return "Enter 提交 · Esc 取消 · Ctrl+U 清空".into();
        }
        if self.pending.is_some() {
            return "Enter 确认 · Esc 取消".into();
        }
        if self.search.is_some() {
            return "Enter 定位 · Esc 取消".into();
        }
        if self.view == View::Help {
            return "↑↓ 滚动 · Esc 返回".into();
        }
        let mut parts = vec![match self.view {
            View::Categories => "↑↓ · Enter 进入",
            View::Problems if self.problems.is_empty() => "暂无问题",
            View::Problems => "↑↓ · Enter 定位",
            _ => match self.current_row().map(|row| &row.action) {
                Some(RowAction::Expand) => "↑↓ · Enter 展开/折叠",
                Some(RowAction::Toggle) => "↑↓ 移动 · Tab 切换",
                Some(RowAction::OpenChoices) => "Tab 切换 · Enter 选择",
                Some(RowAction::Edit(_)) => "↑↓ 移动 · Enter 编辑",
                Some(RowAction::AddItem { .. }) => "↑↓ 移动 · Enter 新增",
                Some(RowAction::Choose { .. }) => "↑↓ 移动 · Enter 选中",
                _ => "↑↓ 移动",
            },
        }];
        if self.dirty_count() > 0 || self.setup_mode {
            parts.push("s 保存");
        }
        if self.view == View::Tree && self.current_row().is_some_and(|row| self.is_list_item(row)) {
            parts.push("J/K 排序 · d 删除");
        }
        if self.view != View::Problems && !self.problems.is_empty() {
            parts.push("p 问题");
        }
        if self.stale {
            parts.push("R 读取磁盘");
        }
        if !self.restart_required.is_empty() && !self.setup_mode {
            parts.push("Ctrl+R 重启网关");
        }
        parts.push("/ 搜索 · ? 帮助");
        parts.push(if self.view == View::Categories {
            "Esc 关闭"
        } else {
            "Esc 返回"
        });
        parts.join(" · ")
    }

    fn root_catalogs(&self) -> Vec<(Root, &SettingNode)> {
        let mut roots = vec![(Root::Gateway, &self.gateway_catalog)];
        if let Some(catalog) = &self.interface_catalog {
            roots.push((Root::Interface, catalog));
        }
        roots
    }

    fn catalog(&self, root: Root) -> Option<&SettingNode> {
        match root {
            Root::Gateway => Some(&self.gateway_catalog),
            Root::Interface => self.interface_catalog.as_ref(),
        }
    }

    /// 按具体路径找目录节点（union 列表用文档里的实际值挑形态）。
    fn node(&self, root: Root, path: &str) -> Option<&SettingNode> {
        tree::concrete_node(self.catalog(root)?, &self.doc, root, path)
    }

    pub fn row_node(&self, row: &Row) -> Option<&SettingNode> {
        let path = match row.action {
            RowAction::Choose { .. } => row.path.rsplit_once('=')?.0,
            _ => &row.path,
        };
        self.node(row.root, path)
    }

    pub fn current_row(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    /// 当前行的身份（`(根, 具体路径)`）——重算后把光标锚回同一行。
    fn row_anchor(&self) -> Option<(Root, String)> {
        self.current_row().map(|row| (row.root, row.path.clone()))
    }

    fn row_index(&self, root: Root, path: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.root == root && row.path == path)
    }

    /// 设置项重算（当前分组的行）+ 光标重锚：先按身份找回原来那一行，
    /// 找不到再交给 kernel 钳制。
    fn rebuild(&mut self, anchor: Option<(Root, String)>) {
        self.refresh_problems();
        self.rows = self.flatten_all();
        match anchor {
            Some((root, path)) if self.row_index(root, &path).is_some() => {
                self.cursor = self.row_index(root, &path).expect("just checked");
            }
            _ => self.clamp_after_refresh(),
        }
    }

    fn flatten_all(&self) -> Vec<Row> {
        let groups = if self.filter.is_some() {
            self.anchors.as_slice()
        } else {
            self.group().map_or(&[][..], std::slice::from_ref)
        };
        groups
            .iter()
            .flat_map(|group| {
                let Some(catalog) = self.catalog(group.root) else {
                    return Vec::new();
                };
                flatten(
                    catalog,
                    group,
                    &self.doc,
                    &self.expanded,
                    &self.problems,
                    self.filter.as_ref(),
                    self.choices.as_ref(),
                )
            })
            .collect()
    }

    fn refresh_problems(&mut self) {
        let local = problems::local_problems(&self.doc, &self.root_catalogs());
        self.problems = problems::merge(local, &self.backend_problems);
        if self.problem_cursor >= self.problems.len() {
            self.problem_cursor = self.problems.len().saturating_sub(1);
        }
    }

    fn recompute_filter(&mut self) {
        let query = self
            .search
            .as_ref()
            .map(|state| state.query().trim().to_string())
            .filter(|query| !query.is_empty());
        self.filter = query.map(|query| SearchFilter::new(&query, &self.root_catalogs()));
    }

    fn prune_expanded(&mut self) {
        let kept: HashSet<(Root, String)> = self
            .expanded
            .iter()
            .filter(|(root, path)| path.is_empty() || self.node(*root, path).is_some())
            .cloned()
            .collect();
        self.expanded = kept;
    }

    /// 值改写 + 标脏 + 重算；Interface 根追加一次实时预览。
    fn write_value(
        &mut self,
        root: Root,
        path: &str,
        value: Value,
        anchor: Option<(Root, String)>,
    ) -> SettingsAction {
        if !self.model_edits.write(&mut self.doc, root, path, &value) {
            self.doc.set_value(root, path, value);
            self.doc.mark_dirty(root, path);
        }
        if self
            .choices
            .as_ref()
            .is_some_and(|choices| choices.root == root && choices.path == path)
        {
            self.choices = None;
        }
        self.rebuild(anchor);
        self.preview_if_interface(root)
    }

    fn preview_if_interface(&self, root: Root) -> SettingsAction {
        if root == Root::Interface {
            SettingsAction::PreviewInterface(self.doc.root_doc(Root::Interface).clone())
        } else {
            SettingsAction::None
        }
    }

    fn remap_sets(&mut self, root: Root, list_path: &str, map: &dyn Fn(usize) -> Option<usize>) {
        self.doc.remap_dirty(root, list_path, map);
        self.model_edits.remap(root, list_path, map);
        self.expanded = self
            .expanded
            .iter()
            .filter_map(|(set_root, path)| {
                if *set_root != root {
                    return Some((*set_root, path.clone()));
                }
                doc_edit::remap_index(path, list_path, map).map(|mapped| (*set_root, mapped))
            })
            .collect();
        let mut close_choices = false;
        if let Some(choices) = self.choices.as_mut()
            && choices.root == root
        {
            match doc_edit::remap_index(&choices.path, list_path, map) {
                Some(mapped) => choices.path = mapped,
                None => close_choices = true,
            }
        }
        if close_choices {
            self.choices = None;
        }
    }

    fn is_list_item(&self, row: &Row) -> bool {
        self.catalog(row.root).is_some_and(|catalog| {
            list::list_item_parent(catalog, &self.doc, row.root, &row.path).is_some()
        })
    }

    // ── 按键：帮助 / 搜索 / 问题清单 ─────────────────────────

    fn handle_help_key(&mut self, key: KeyEvent) -> SettingsAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') => self.view = self.help_back,
            KeyCode::Up => self.help_offset = self.help_offset.saturating_sub(1),
            KeyCode::Down => self.help_offset = (self.help_offset + 1).min(self.help_last_offset),
            _ => {}
        }
        SettingsAction::None
    }

    pub fn help_offset(&self) -> usize {
        self.help_offset
    }

    pub fn set_help_last_offset(&mut self, offset: usize) {
        self.help_last_offset = offset;
        self.help_offset = self.help_offset.min(offset);
    }

    fn open_help(&mut self) {
        self.help_back = self.view;
        self.view = View::Help;
        self.help_offset = 0;
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> SettingsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.exit_search_restore(),
            KeyCode::Enter => {
                if let Some((root, path)) = self.row_anchor() {
                    if let Some(group) = self.group_index_for(root, &path) {
                        self.group_cursor = group;
                    }
                    for ancestor in doc_edit::ancestors(&path) {
                        self.expanded.insert((root, ancestor));
                    }
                    self.search = None;
                    self.recompute_filter();
                    self.rebuild(Some((root, path)));
                }
            }
            KeyCode::Up => {
                self.move_cursor(-1);
            }
            KeyCode::Down => {
                self.move_cursor(1);
            }
            KeyCode::PageUp => {
                self.move_cursor(-(self.viewport_rows as isize));
            }
            KeyCode::PageDown => {
                self.move_cursor(self.viewport_rows as isize);
            }
            KeyCode::Home => self.set_cursor_at(0, 0),
            KeyCode::End => {
                let last = self.rows.len().saturating_sub(1);
                self.set_cursor_at(0, last);
            }
            KeyCode::Backspace => {
                if let Some(state) = self.search.as_mut() {
                    state.backspace();
                }
                self.rebuild_search();
            }
            KeyCode::Char('u' | 'U') if ctrl => {
                if let Some(state) = self.search.as_mut() {
                    state.clear();
                }
                self.rebuild_search();
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(state) = self.search.as_mut() {
                    state.push(c);
                }
                self.rebuild_search();
            }
            _ => {}
        }
        SettingsAction::None
    }

    fn enter_search(&mut self) {
        self.search_back = self.view;
        self.view = View::Tree;
        let cursor = self.row_anchor();
        self.search = Some(SearchState::new(&self.expanded, cursor, self.group_cursor));
        self.choices = None;
        self.enter_group(self.group_cursor);
    }

    fn rebuild_search(&mut self) {
        self.recompute_filter();
        self.rebuild(None);
        if let Some(index) = self.first_hit_row() {
            self.cursor = index;
        }
    }

    fn first_hit_row(&self) -> Option<usize> {
        let filter = self.filter.as_ref()?;
        self.rows
            .iter()
            .position(|row| filter.is_hit(row.root, &row.tpath))
    }

    /// Esc：清空 query 并回到搜索前的展开快照、分组与光标。
    fn exit_search_restore(&mut self) {
        let Some(state) = self.search.take() else {
            return;
        };
        self.view = self.search_back;
        self.expanded = state.snapshot().clone();
        let group = state.group_snapshot();
        self.recompute_filter();
        let anchor = state.cursor_snapshot().or_else(|| self.row_anchor());
        if group != self.group_cursor && group < self.anchors.len() {
            self.group_cursor = group;
        }
        self.rebuild(anchor);
    }

    fn open_problems(&mut self) {
        if self.view != View::Problems {
            self.problems_back = self.view;
            self.view = View::Problems;
        }
    }

    fn handle_problems_key(&mut self, key: KeyEvent) -> SettingsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('p') => self.view = self.problems_back,
            KeyCode::Char('R') if !ctrl => return self.discard_then(DiscardIntent::Reload),
            KeyCode::Char('r' | 'R') if ctrl => return self.restart_action(),
            KeyCode::Char('s') if !ctrl => return self.save_action(),
            KeyCode::Char('/') => self.enter_search(),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Up => self.move_problem_cursor(-1),
            KeyCode::Down => self.move_problem_cursor(1),
            KeyCode::PageUp => self.move_problem_cursor(-(self.viewport_rows as isize)),
            KeyCode::PageDown => self.move_problem_cursor(self.viewport_rows as isize),
            KeyCode::Home => self.problem_cursor = 0,
            KeyCode::End => self.problem_cursor = self.problems.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Right => {
                self.jump_to_problem();
            }
            _ => {}
        }
        SettingsAction::None
    }

    fn move_problem_cursor(&mut self, delta: isize) {
        if self.problems.is_empty() {
            self.problem_cursor = 0;
            return;
        }
        let next =
            (self.problem_cursor as isize + delta).clamp(0, self.problems.len() as isize - 1);
        self.problem_cursor = next as usize;
    }

    /// `Enter` on 问题清单：回树、**切到问题所在的分组**、展开祖先、光标落行。
    /// `path == None`（文档级问题）→ 无操作。
    fn jump_to_problem(&mut self) -> bool {
        let Some(problem) = self.problems.get(self.problem_cursor) else {
            return false;
        };
        let Some(path) = problem.path.clone() else {
            return false;
        };
        let root = problem.root;
        if let Some(index) = self.group_index_for(root, &path) {
            self.enter_group(index);
        }
        self.view = View::Tree;
        for ancestor in doc_edit::ancestors(&path) {
            self.expanded.insert((root, ancestor));
        }
        self.rebuild(Some((root, path.clone())));
        self.current_row()
            .is_some_and(|row| row.root == root && row.path == path)
    }

    // ── 按键：分类与设置项 ───────────────────────────────────

    fn handle_tree_key(&mut self, key: KeyEvent) -> SettingsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return self.escape_tree(),
            KeyCode::Char('s') if !ctrl => return self.save_action(),
            KeyCode::Char('r' | 'R') if ctrl => return self.restart_action(),
            KeyCode::Char('R') => return self.discard_then(DiscardIntent::Reload),
            KeyCode::Char('/') => {
                self.enter_search();
                return SettingsAction::None;
            }
            KeyCode::Char('p') => {
                self.open_problems();
                return SettingsAction::None;
            }
            KeyCode::Char('?') => {
                self.open_help();
                return SettingsAction::None;
            }
            _ => {}
        }
        if self.view == View::Categories {
            self.handle_categories_key(key)
        } else {
            self.handle_items_key(key, ctrl)
        }
    }

    fn handle_categories_key(&mut self, key: KeyEvent) -> SettingsAction {
        match key.code {
            KeyCode::Up => self.move_group(-1),
            KeyCode::Down => self.move_group(1),
            KeyCode::PageUp => self.move_group(-(self.viewport_rows as isize)),
            KeyCode::PageDown => self.move_group(self.viewport_rows as isize),
            KeyCode::Home => self.group_cursor = 0,
            KeyCode::End => self.group_cursor = self.anchors.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Right if self.group().is_some() => {
                self.view = View::Tree;
                self.cursor = 0;
                self.enter_group(self.group_cursor);
            }
            _ => {}
        }
        SettingsAction::None
    }

    fn handle_items_key(&mut self, key: KeyEvent, ctrl: bool) -> SettingsAction {
        match key.code {
            KeyCode::Up if self.choices.is_some() && self.cursor_on_choice_owner() => {
                self.move_choice_cursor(-1);
            }
            KeyCode::Down if self.choices.is_some() && self.cursor_on_choice_owner() => {
                self.move_choice_cursor(1);
            }
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-(self.viewport_rows as isize)),
            KeyCode::PageDown => self.move_cursor(self.viewport_rows as isize),
            KeyCode::Home => self.set_cursor_at(0, 0),
            KeyCode::End => {
                let last = self.rows.len().saturating_sub(1);
                self.set_cursor_at(0, last);
            }
            KeyCode::Left => return self.left_action(),
            KeyCode::Right => return self.right_action(),
            KeyCode::Enter => return self.enter_action(),
            KeyCode::Tab if key.modifiers.is_empty() => return self.tab_action(),
            KeyCode::Char(' ') => return self.space_action(),
            KeyCode::Char('a') if !ctrl => return self.start_add(),
            KeyCode::Char('d') if !ctrl => return self.delete_action(),
            KeyCode::Char('J') if !ctrl => return self.move_item_action(1),
            KeyCode::Char('K') if !ctrl => return self.move_item_action(-1),
            KeyCode::Char('r') => return self.reset_action(),
            _ => {}
        }
        SettingsAction::None
    }

    fn tab_action(&mut self) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            return SettingsAction::None;
        };
        match row.action {
            RowAction::Toggle => self.toggle_bool(&row),
            RowAction::OpenChoices => self.cycle_enum(&row, 1),
            _ => SettingsAction::None,
        }
    }

    fn escape_tree(&mut self) -> SettingsAction {
        if let Some(choices) = self.choices.take() {
            self.rebuild(Some((choices.root, choices.path)));
            return SettingsAction::None;
        }
        if self.view == View::Tree && self.group().is_some() {
            self.view = View::Categories;
            return SettingsAction::None;
        }
        if self.dirty_count() > 0 {
            self.pending = Some(Pending::Discard {
                intent: DiscardIntent::Close,
            });
            return SettingsAction::None;
        }
        SettingsAction::Close { discard: false }
    }

    fn cursor_on_choice_owner(&self) -> bool {
        let Some(choices) = self.choices.as_ref() else {
            return false;
        };
        self.current_row()
            .is_some_and(|row| row.root == choices.root && row.path == choices.path)
    }

    /// 设置项列表的 `←`：折叠 / 跳父行 / enum 往前切值；**已到分组顶层（无处可去）则返回分类**。
    fn left_action(&mut self) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            self.view = View::Categories;
            return SettingsAction::None;
        };
        if self
            .choices
            .as_ref()
            .is_some_and(|choices| choices.root == row.root && choices.path == row.path)
        {
            self.choices = None;
            let anchor = self.row_anchor();
            self.rebuild(anchor);
            return SettingsAction::None;
        }
        match row.action.clone() {
            RowAction::OpenChoices => self.cycle_enum(&row, -1),
            RowAction::Expand => {
                if self.expanded.contains(&(row.root, row.path.clone())) {
                    self.expanded.remove(&(row.root, row.path.clone()));
                    self.rebuild(Some((row.root, row.path.clone())));
                } else {
                    self.left_or_out(row.depth);
                }
                SettingsAction::None
            }
            RowAction::Choose { .. } => {
                self.choices = None;
                let anchor = self.row_anchor();
                self.rebuild(anchor);
                SettingsAction::None
            }
            _ => {
                self.left_or_out(row.depth);
                SettingsAction::None
            }
        }
    }

    /// `←` selects a parent row or returns to the category list.
    fn left_or_out(&mut self, depth: usize) {
        if depth > 0 {
            self.goto_parent();
        } else {
            self.view = View::Categories;
        }
    }

    fn right_action(&mut self) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            return SettingsAction::None;
        };
        match row.action.clone() {
            RowAction::OpenChoices => self.cycle_enum(&row, 1),
            RowAction::Expand => {
                let key = (row.root, row.path.clone());
                if !self.expanded.contains(&key) {
                    self.expanded.insert(key);
                    self.rebuild(Some((row.root, row.path.clone())));
                }
                SettingsAction::None
            }
            _ => SettingsAction::None,
        }
    }

    fn goto_parent(&mut self) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        let mut index = self.cursor;
        while index > 0 {
            index -= 1;
            let candidate = &self.rows[index];
            if candidate.depth < row.depth && doc_edit::path_is_within(&candidate.path, &row.path) {
                self.cursor = index;
                return;
            }
        }
    }

    fn enter_action(&mut self) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            return SettingsAction::None;
        };
        match row.action.clone() {
            RowAction::Expand => {
                let key = (row.root, row.path.clone());
                if !self.expanded.remove(&key) {
                    self.expanded.insert(key);
                }
                self.rebuild(Some((row.root, row.path.clone())));
                SettingsAction::None
            }
            RowAction::Toggle => self.toggle_bool(&row),
            RowAction::OpenChoices => {
                // 已展开：Enter = 选中内项光标所在的选择项（含 `(unset)`）。
                let selected = self
                    .choices
                    .as_ref()
                    .filter(|choices| choices.root == row.root && choices.path == row.path)
                    .map(|choices| self.choice_value_at(&row, choices.cursor));
                match selected {
                    Some(value) => self.select_choice(&row, value),
                    None => {
                        self.open_choices(&row);
                        SettingsAction::None
                    }
                }
            }
            RowAction::Edit(kind) => {
                self.open_editor(&row, kind);
                SettingsAction::None
            }
            RowAction::AddItem { variants } => self.add_at(&row, variants),
            RowAction::Choose { value, .. } => self.select_choice(&row, value),
            RowAction::ReadOnly => SettingsAction::None,
        }
    }

    fn space_action(&mut self) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            return SettingsAction::None;
        };
        match row.action.clone() {
            RowAction::Toggle => self.toggle_bool(&row),
            RowAction::Choose { value, .. } => self.select_choice(&row, value),
            _ => SettingsAction::None,
        }
    }

    // ── 值编辑动作 ───────────────────────────────────────────

    fn toggle_bool(&mut self, row: &Row) -> SettingsAction {
        let Some(node) = self.node(row.root, &row.path) else {
            return SettingsAction::None;
        };
        if node.kind != SettingKind::Bool || !node.editable {
            return SettingsAction::None;
        }
        let current = self
            .doc
            .value(row.root, &row.path)
            .and_then(Value::as_bool)
            .or_else(|| node.default.as_ref().and_then(Value::as_bool))
            .unwrap_or(false);
        self.write_value(
            row.root,
            &row.path,
            json!(!current),
            Some((row.root, row.path.clone())),
        )
    }

    fn open_choices(&mut self, row: &Row) {
        let Some(node) = self.node(row.root, &row.path).cloned() else {
            return;
        };
        if node.kind != SettingKind::Enum || node.choices.is_empty() && !node.nullable {
            return;
        }
        let cursor = enum_cursor(&node, self.doc.value(row.root, &row.path));
        self.choices = Some(ChoiceState {
            root: row.root,
            path: row.path.clone(),
            cursor,
        });
        self.rebuild(Some((row.root, row.path.clone())));
    }

    fn move_choice_cursor(&mut self, delta: isize) {
        let Some((root, path, cursor)) = self
            .choices
            .as_ref()
            .map(|choices| (choices.root, choices.path.clone(), choices.cursor))
        else {
            return;
        };
        let Some(node) = self.node(root, &path) else {
            return;
        };
        let len = node.choices.len() + usize::from(node.nullable);
        if len == 0 {
            return;
        }
        let next = wrap_index(cursor, delta, len);
        if let Some(choices) = self.choices.as_mut() {
            choices.cursor = next;
        }
        let anchor = self.row_anchor();
        self.rebuild(anchor);
    }

    /// 内项光标 `cursor` 对应的 wire 值（`None` = `(unset)` 行）。
    fn choice_value_at(&self, row: &Row, cursor: usize) -> Option<String> {
        let node = self.node(row.root, &row.path)?;
        node.choices.get(cursor).map(|choice| choice.value.clone())
    }

    fn select_choice(&mut self, _row: &Row, value: Option<String>) -> SettingsAction {
        let Some(choices) = self.choices.clone() else {
            return SettingsAction::None;
        };
        let Some(node) = self.node(choices.root, &choices.path).cloned() else {
            return SettingsAction::None;
        };
        let value = value.map_or(Value::Null, Value::String);
        self.choices = None;
        // 选择项只在 enum 行上展开；写回 enum 自己的路径。
        let _ = node;
        self.write_value(
            choices.root,
            &choices.path,
            value,
            Some((choices.root, choices.path.clone())),
        )
    }

    /// enum 行上的 `←` / `→`：在 choices（+ nullable 的 `(unset)`）之间循环切值。
    fn cycle_enum(&mut self, row: &Row, delta: isize) -> SettingsAction {
        let Some(node) = self.node(row.root, &row.path).cloned() else {
            return SettingsAction::None;
        };
        if node.kind != SettingKind::Enum || !node.editable {
            return SettingsAction::None;
        }
        let len = node.choices.len() + usize::from(node.nullable);
        if len == 0 {
            return SettingsAction::None;
        }
        // 缺席时没有「当前项」：→ 落在第一项、← 落在最后一项（不是从 0 再走一步）。
        let next = match self
            .doc
            .value(row.root, &row.path)
            .or(node.default.as_ref())
        {
            None => {
                if delta > 0 {
                    0
                } else {
                    len - 1
                }
            }
            value => wrap_index(enum_cursor(&node, value), delta, len),
        };
        let value = if node.nullable && next == node.choices.len() {
            Value::Null
        } else {
            Value::String(node.choices[next].value.clone())
        };
        self.write_value(
            row.root,
            &row.path,
            value,
            Some((row.root, row.path.clone())),
        )
    }

    fn open_editor(&mut self, row: &Row, _kind: ScalarKind) {
        let Some(node) = self.node(row.root, &row.path).cloned() else {
            return;
        };
        self.open_editor_at(row.root, &row.path, &node);
    }

    fn open_editor_at(&mut self, root: Root, path: &str, node: &SettingNode) {
        let Some(kind) = edit::editor_kind(node) else {
            return;
        };
        let initial = if kind == ScalarKind::Secret {
            String::new()
        } else {
            self.initial_buffer(root, path, node, kind)
        };
        self.edit = Some(EditState::open(
            root,
            path.to_string(),
            kind,
            node.required,
            Constraints::from_node(node),
            initial,
        ));
    }

    fn initial_buffer(
        &self,
        root: Root,
        path: &str,
        node: &SettingNode,
        kind: ScalarKind,
    ) -> String {
        match model::field_value(&self.doc, root, path) {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Null) => String::new(),
            Some(value) => tree::render_scalar(value),
            None => match node.default.as_ref().filter(|default| !default.is_null()) {
                Some(default) => tree::render_scalar(default),
                None if kind == ScalarKind::Json => "{}".to_string(),
                None => String::new(),
            },
        }
    }

    // ── 列表动作 ─────────────────────────────────────────────

    fn start_add(&mut self) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            return SettingsAction::None;
        };
        self.add_at(&row, usize::MAX)
    }

    /// `a` / `Enter` on `(+ 新增一项)`：`variants` 是这一行声明的元素形态数
    /// （`a` 走 `usize::MAX`，与行无关，统一由这里解析最近列表）。
    fn add_at(&mut self, row: &Row, variants: usize) -> SettingsAction {
        let Some(catalog) = self.catalog(row.root) else {
            return SettingsAction::None;
        };
        let Some(list_path) = list::nearest_list(catalog, &row.path) else {
            return SettingsAction::None;
        };
        let Some(list_node) = self.node(row.root, &list_path).cloned() else {
            return SettingsAction::None;
        };
        if !list_node.editable || list_node.apply == ApplyScope::Readonly {
            return SettingsAction::None;
        }
        let count = match self.doc.value(row.root, &list_path) {
            Some(Value::Array(items)) => items.len(),
            _ => 0,
        };
        if list_node.max_items.is_some_and(|max| (count as i64) >= max) {
            return SettingsAction::None;
        }
        if let Some(variant) = model::object_variant(row.root, &list_node) {
            return self.finish_add(row.root, &list_path, &list_node, Some(variant));
        }
        let declared = list_node
            .variants
            .as_ref()
            .map_or_else(|| usize::from(list_node.element.is_some()), Vec::len);
        if declared == 0 {
            return SettingsAction::None;
        }
        let forms = if variants == usize::MAX {
            declared
        } else {
            variants
        };
        if forms > 1 {
            self.pending = Some(Pending::Variants {
                root: row.root,
                list_path,
                cursor: 0,
            });
            return SettingsAction::None;
        }
        self.finish_add(
            row.root,
            &list_path,
            &list_node,
            list_node.variants.as_ref().map(|_| 0),
        )
    }

    fn finish_add(
        &mut self,
        root: Root,
        list_path: &str,
        list_node: &SettingNode,
        variant: Option<usize>,
    ) -> SettingsAction {
        let Some(outcome) = list::add_item(&mut self.doc, root, list_path, list_node, variant)
        else {
            return SettingsAction::None;
        };
        self.doc.mark_dirty(root, list_path);
        self.expanded.insert((root, list_path.to_string()));
        self.expanded.insert((root, outcome.item_path.clone()));
        self.rebuild(Some((root, outcome.cursor_at.clone())));
        let edit_at = outcome
            .edit_at
            .or_else(|| model::object_variant(root, list_node).map(|_| outcome.cursor_at));
        if let Some(edit_path) = edit_at
            && let Some(node) = self.node(root, &edit_path).cloned()
        {
            self.open_editor_at(root, &edit_path, &node);
        }
        self.preview_if_interface(root)
    }

    fn delete_action(&mut self) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            return SettingsAction::None;
        };
        let is_item = self.catalog(row.root).is_some_and(|catalog| {
            list::list_item_parent(catalog, &self.doc, row.root, &row.path).is_some()
        });
        if is_item {
            let Some((list_path, index)) = self.catalog(row.root).and_then(|catalog| {
                list::list_item_parent(catalog, &self.doc, row.root, &row.path)
            }) else {
                return SettingsAction::None;
            };
            self.pending = Some(Pending::DeleteItem {
                root: row.root,
                list_path,
                index,
                label: row.label.clone(),
            });
            return SettingsAction::None;
        }
        if matches!(
            row.action,
            RowAction::Edit(_) | RowAction::Toggle | RowAction::OpenChoices
        ) {
            self.pending = Some(Pending::ClearScalar {
                root: row.root,
                path: row.path.clone(),
                label: row.label.clone(),
            });
        }
        SettingsAction::None
    }

    fn move_item_action(&mut self, delta: isize) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            return SettingsAction::None;
        };
        let Some((list_path, index)) = self
            .catalog(row.root)
            .and_then(|catalog| list::list_item_parent(catalog, &self.doc, row.root, &row.path))
        else {
            return SettingsAction::None;
        };
        let Some(list_node) = self.node(row.root, &list_path).cloned() else {
            return SettingsAction::None;
        };
        if !list_node.editable || list_node.apply == ApplyScope::Readonly {
            return SettingsAction::None;
        }
        let Some(new_index) = list::move_item(&mut self.doc, row.root, &list_path, index, delta)
        else {
            return SettingsAction::None;
        };
        self.doc.mark_dirty(row.root, &list_path);
        let map = doc_edit::swap_index_map(index, new_index);
        self.remap_sets(row.root, &list_path, &map);
        let new_path = doc_edit::index_path(&list_path, new_index);
        self.rebuild(Some((row.root, new_path)));
        self.preview_if_interface(row.root)
    }

    fn reset_action(&mut self) -> SettingsAction {
        let Some(row) = self.current_row().cloned() else {
            return SettingsAction::None;
        };
        // 复位只对叶子字段有定义：列表项、结构行、只读行都无操作。
        if self.is_list_item(&row)
            || !matches!(
                row.action,
                RowAction::Edit(_) | RowAction::Toggle | RowAction::OpenChoices
            )
        {
            return SettingsAction::None;
        }
        if !self.doc.contains(row.root, &row.path) {
            return SettingsAction::None;
        }
        self.reset_value(row.root, &row.path);
        if self
            .choices
            .as_ref()
            .is_some_and(|choices| choices.root == row.root && choices.path == row.path)
        {
            self.choices = None;
        }
        self.rebuild(Some((row.root, row.path.clone())));
        self.preview_if_interface(row.root)
    }

    fn reset_value(&mut self, root: Root, path: &str) {
        if !self.model_edits.reset(&mut self.doc, root, path) {
            self.doc.remove_value(root, path);
            self.doc.mark_dirty(root, path);
        }
    }

    // ── 保存 / 重载 / 重启 / 关闭 ────────────────────────────

    fn save_action(&mut self) -> SettingsAction {
        let gateway_dirty = self.doc.dirty_count_in(Root::Gateway) > 0;
        let interface_dirty = self.doc.dirty_count_in(Root::Interface) > 0;
        if !gateway_dirty && !interface_dirty {
            return SettingsAction::None;
        }
        SettingsAction::Save {
            gateway: self.doc.root_doc(Root::Gateway).clone(),
            base: self.doc.fingerprint().to_string(),
            interface: self.doc.root_doc(Root::Interface).clone(),
            gateway_dirty,
            interface_dirty,
        }
    }

    /// `Ctrl+R`：只在**真的有待重启的变更**（上次保存回执的 `restart_required` 非空）时接受。
    /// 没有 → 无操作 —— 键位栏同样不显示它，免得教用户按一个没有意义的键。
    fn restart_action(&mut self) -> SettingsAction {
        if self.restart_required.is_empty() {
            return SettingsAction::None;
        }
        self.discard_then(DiscardIntent::Restart)
    }

    fn discard_then(&mut self, intent: DiscardIntent) -> SettingsAction {
        if self.dirty_count() > 0 {
            self.pending = Some(Pending::Discard { intent });
            return SettingsAction::None;
        }
        self.perform_discard(intent)
    }

    fn perform_discard(&mut self, intent: DiscardIntent) -> SettingsAction {
        match intent {
            DiscardIntent::Close => SettingsAction::Close { discard: true },
            DiscardIntent::Reload => SettingsAction::Reload,
            DiscardIntent::Restart => SettingsAction::RestartGateway,
        }
    }

    // ── 编辑器 / 模态提示 ────────────────────────────────────

    fn handle_edit_key(&mut self, key: KeyEvent) -> SettingsAction {
        let event = self.edit.as_mut().map(|edit| edit.handle_key(key));
        match event {
            None | Some(EditEvent::None) => SettingsAction::None,
            Some(EditEvent::Cancel) => {
                self.edit = None;
                SettingsAction::None
            }
            Some(EditEvent::Submit) => {
                let Some((root, path, result)) = self
                    .edit
                    .as_ref()
                    .map(|edit| (edit.root(), edit.path().to_string(), edit.validate()))
                else {
                    return SettingsAction::None;
                };
                match result {
                    Ok(value) => {
                        // 「打开 → 不改 → Enter」= 无操作（不写入、不标脏）——
                        // 否则在一个「缺席即默认」的字段上连按两次 Enter 会把默认值物化。
                        let unchanged = self
                            .edit
                            .as_ref()
                            .is_some_and(|edit| edit.commits_unchanged(&value));
                        self.edit = None;
                        if unchanged {
                            SettingsAction::None
                        } else {
                            self.write_value(root, &path, value, Some((root, path.clone())))
                        }
                    }
                    Err(message) => {
                        if let Some(edit) = self.edit.as_mut() {
                            edit.set_error(message);
                        }
                        SettingsAction::None
                    }
                }
            }
        }
    }

    fn handle_pending_key(&mut self, key: KeyEvent) -> SettingsAction {
        match key.code {
            KeyCode::Enter => self.confirm_pending(),
            KeyCode::Esc => {
                self.pending = None;
                SettingsAction::None
            }
            KeyCode::Up => {
                self.move_variant_cursor(-1);
                SettingsAction::None
            }
            KeyCode::Down => {
                self.move_variant_cursor(1);
                SettingsAction::None
            }
            _ => SettingsAction::None,
        }
    }

    fn move_variant_cursor(&mut self, delta: isize) {
        let Some((root, list_path, cursor)) = (match self.pending.as_ref() {
            Some(Pending::Variants {
                root,
                list_path,
                cursor,
            }) => Some((*root, list_path.clone(), *cursor)),
            _ => None,
        }) else {
            return;
        };
        let len = self
            .node(root, &list_path)
            .and_then(|node| node.variants.as_ref().map(Vec::len))
            .unwrap_or(0);
        if len == 0 {
            return;
        }
        let next = (cursor as isize + delta).clamp(0, len as isize - 1) as usize;
        if let Some(Pending::Variants { cursor, .. }) = self.pending.as_mut() {
            *cursor = next;
        }
    }

    fn confirm_pending(&mut self) -> SettingsAction {
        let Some(pending) = self.pending.take() else {
            return SettingsAction::None;
        };
        match pending {
            Pending::DeleteItem {
                root,
                list_path,
                index,
                ..
            } => {
                if !list::remove_item(&mut self.doc, root, &list_path, index) {
                    return SettingsAction::None;
                }
                self.doc.mark_dirty(root, &list_path);
                let map = doc_edit::remove_index_map(index);
                self.remap_sets(root, &list_path, &map);
                let anchor = self.deletion_anchor(root, &list_path, index);
                self.rebuild(Some(anchor));
                self.preview_if_interface(root)
            }
            Pending::ClearScalar { root, path, .. } => {
                self.reset_value(root, &path);
                self.rebuild(Some((root, path)));
                self.preview_if_interface(root)
            }
            Pending::Variants {
                root,
                list_path,
                cursor,
            } => {
                let Some(list_node) = self.node(root, &list_path).cloned() else {
                    return SettingsAction::None;
                };
                self.finish_add(root, &list_path, &list_node, Some(cursor))
            }
            Pending::Discard { intent } => self.perform_discard(intent),
        }
    }

    /// 删除之后光标落到「补位的那一项」，末项被删则落到 `(+ 新增一项)` 行。
    fn deletion_anchor(&self, root: Root, list_path: &str, index: usize) -> (Root, String) {
        let count = match self.doc.value(root, list_path) {
            Some(Value::Array(items)) => items.len(),
            _ => 0,
        };
        if index < count {
            (root, doc_edit::index_path(list_path, index))
        } else {
            (root, format!("{list_path}[]"))
        }
    }

    fn prompt_view(&self) -> Option<PromptView> {
        let pending = self.pending.as_ref()?;
        Some(match pending {
            Pending::DeleteItem {
                root,
                list_path,
                index,
                label,
            } => {
                let mut lines = Vec::new();
                if let Some(node) = self.node(*root, list_path) {
                    let count = match self.doc.value(*root, list_path) {
                        Some(Value::Array(items)) => items.len(),
                        _ => 0,
                    };
                    if let Some(min) = node.min_items
                        && ((count.saturating_sub(1)) as i64) < min
                    {
                        lines.push(format!("列表至少需要 {min} 项，保存时会被拒绝"));
                    }
                    if node.apply != ApplyScope::Hot {
                        lines.push(apply_note(&node.apply));
                    }
                }
                let _ = index;
                PromptView {
                    kind: PromptKind::Confirm,
                    title: format!("删除 \"{label}\"？"),
                    lines,
                    options: Vec::new(),
                    cursor: 0,
                }
            }
            Pending::ClearScalar { label, .. } => PromptView {
                kind: PromptKind::Confirm,
                title: format!("清空 \"{label}\"？"),
                lines: vec!["该键会从 config.yaml 移除，回到声明默认值".into()],
                options: Vec::new(),
                cursor: 0,
            },
            Pending::Variants {
                root,
                list_path,
                cursor,
            } => {
                let options = self
                    .node(*root, list_path)
                    .and_then(|node| node.variants.as_ref())
                    .map(|variants| {
                        variants
                            .iter()
                            .map(|variant| {
                                if variant.doc.is_empty() {
                                    variant.display_label().to_string()
                                } else {
                                    format!("{} — {}", variant.display_label(), variant.doc)
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                PromptView {
                    kind: PromptKind::Variants,
                    title: "新增一项 — 选择形态".into(),
                    lines: Vec::new(),
                    options,
                    cursor: *cursor,
                }
            }
            Pending::Discard { intent } => PromptView {
                kind: PromptKind::Confirm,
                title: match intent {
                    DiscardIntent::Close => {
                        format!("放弃 {} 项未保存的改动？", self.dirty_count())
                    }
                    DiscardIntent::Reload => {
                        format!("放弃 {} 项未保存的改动并重新载入？", self.dirty_count())
                    }
                    DiscardIntent::Restart => {
                        format!("放弃 {} 项未保存的改动并重启网关？", self.dirty_count())
                    }
                },
                lines: discard_lines(*intent),
                options: Vec::new(),
                cursor: 0,
            },
        })
    }
}

/// 一个具体路径的**首段**（顶层键）：`providers[0].api_key` → `providers`。
///
/// 分组成员是按顶层键声明的，所以「这条路径 / 这个问题 / 这次命中属于哪一组」只看首段。
/// 切分点是 `.` 或 `[`（`providers[0].api_key` → `providers`）：配置键都是标识符，
/// 这一刀与 [`doc_edit::format_path`] 的完整解析在合法路径上等价；切不出分隔符就整串当首段，
/// 于是不匹配任何成员——行为等价于"不属于任何组"，不会 panic 也不会误归。
fn path_head(path: &str) -> &str {
    match path.split_once(['.', '[']) {
        Some((head, _)) => head,
        None => path,
    }
}

fn discard_lines(intent: DiscardIntent) -> Vec<String> {
    match intent {
        DiscardIntent::Close => Vec::new(),
        DiscardIntent::Reload => vec!["面板将重新读取磁盘上的 config.yaml".into()],
        DiscardIntent::Restart => {
            vec!["网关会关闭并重新拉起，进行中的回合会被中断".into()]
        }
    }
}

fn apply_note(apply: &ApplyScope) -> String {
    match apply {
        ApplyScope::NextSession => "生效：新会话（进行中的会话保持现状）".into(),
        ApplyScope::Restart => "生效：需重启网关（Ctrl+R）".into(),
        ApplyScope::Readonly => "只读".into(),
        _ => "保存即生效".into(),
    }
}

/// enum 行的内项光标初始位置：当前值（或默认值）那一项；`(unset)` 是最后一行。
fn enum_cursor(node: &SettingNode, value: Option<&Value>) -> usize {
    let unset = node.choices.len();
    match value {
        Some(Value::String(text)) => node
            .choices
            .iter()
            .position(|choice| choice.value.eq_ignore_ascii_case(text))
            .unwrap_or(0),
        Some(Value::Null) if node.nullable => unset,
        Some(_) => 0,
        None => {
            if let Some(default) = node.default.as_ref().and_then(Value::as_str)
                && let Some(index) = node
                    .choices
                    .iter()
                    .position(|choice| choice.value == default)
            {
                return index;
            }
            if node.nullable && !node.has_default {
                unset
            } else {
                0
            }
        }
    }
}

/// 单页适配器：整棵树就是唯一一页，于是 kernel 的 `move_cursor`（钳制）与
/// `clamp_after_refresh` **白拿**。
impl SelectionPanel for SettingsPanel {
    fn page_count(&self) -> usize {
        1
    }

    fn page_kind(&self, _page: usize) -> PageKind {
        PageKind::Options {
            rows: self.rows.len(),
        }
    }

    fn current_page(&self) -> usize {
        0
    }

    fn set_current_page(&mut self, _page: usize) {}

    fn cursor_at(&self, _page: usize) -> usize {
        self.cursor
    }

    fn set_cursor_at(&mut self, _page: usize, row: usize) {
        self.cursor = row;
    }

    fn committed_at(&self, _page: usize) -> Option<usize> {
        None
    }

    fn set_committed_at(&mut self, _page: usize, _row: Option<usize>) {}
}
