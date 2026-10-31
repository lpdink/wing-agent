//! Popup infrastructure — slash command and sub-command candidate popups.

pub mod command;
pub mod selection;

use crate::shared::session_tree::Crew;
use command::CandidateCache;
use command::PopupAction;
use command::SessionQuery;
use command::candidate_request_for;
use command::filter_candidates;
use command::filter_commands;
use command::is_exact_session_match;
use command::is_local_candidate_command;
use command::is_session_command;
use command::parse_slash_input;
use command::session_panel;
use selection::SelectionRow;
use selection::SelectionState;
use selection::SessionChrome;
use selection::popup_height;
use selection::rich_max_visible;

/// Active popup state.
#[derive(Debug, Clone, Default)]
pub enum ActivePopup {
    /// No popup active.
    #[default]
    None,
    /// Command list popup (showing slash commands).
    Command {
        /// Current filter text (after `/`).
        filter: String,
        /// Filtered command rows.
        rows: Vec<SelectionRow>,
        /// Navigation state.
        state: SelectionState,
    },
    /// Sub-command candidate popup (e.g. model list).
    SubCommand {
        /// The parent command name (e.g. "/model").
        command: String,
        /// Current filter text (after command name + space).
        filter: String,
        /// Filtered candidate rows.
        rows: Vec<SelectionRow>,
        /// Navigation state.
        state: SelectionState,
        /// 层视图的 chrome 行（面包屑 / dormant 尾注）；不是候选行，只占高度。
        chrome: SessionChrome,
    },
}

/// Returns `true` if `args` (case-insensitive) exactly matches any candidate ID.
fn is_exact_candidate_match(candidates: &[(String, String)], args: &str) -> bool {
    let args_lower = args.to_lowercase();
    candidates
        .iter()
        .any(|(id, _)| id.to_lowercase() == args_lower)
}

/// Move selection to the row exactly matching `args` (case-insensitive), if any.
///
/// must-select 命令的 Tab 补全路径：输入被填为精确候选名后 popup 保持打开，
/// 选中位跟随该行，Enter 即确认它。
fn select_exact_match(rows: &[SelectionRow], args: &str, state: &mut SelectionState) {
    if args.is_empty() {
        return;
    }
    if let Some(idx) = rows.iter().position(|r| r.name.eq_ignore_ascii_case(args)) {
        state.select(idx);
    }
}

impl ActivePopup {
    /// Update popup state based on current input text.
    ///
    /// Returns an optional action to fetch candidates via HTTP.
    ///
    /// `query` 只被会话面板消费（层视图的 scope / dormant 开关 / 当前会话）；
    /// 其它命令忽略它。
    pub fn update_from_input(
        &mut self,
        text: &str,
        cache: &CandidateCache,
        query: SessionQuery<'_>,
    ) -> Option<PopupAction> {
        let Some((cmd, args)) = parse_slash_input(text) else {
            *self = Self::None;
            return None;
        };

        // Session commands (`/session`, `/ss`): HTTP-fetched rich candidates.
        if is_session_command(cmd) {
            // 缓存只负责"先渲染旧行"：打开面板的那一刻由输入路径补发一次刷新
            // （#147，见 `App::update_popup_from_input`），不再是"有缓存就不
            // 取数"的判据。**空列表也算抓过**——否则"抓到了 0 条"会退回"等待
            // 首帧"分支，每一次刷新响应都再发一次请求（空列表请求环）。
            if cache.sessions_fetched {
                let candidates = &cache.sessions;
                // must-select 命令精确匹配时保持 popup 打开（Enter = 确认选择）；
                // 非 must-select 命令精确匹配即隐藏。
                if !command::is_must_select_command(cmd)
                    && !args.is_empty()
                    && is_exact_session_match(candidates, args)
                {
                    *self = Self::None;
                    return None;
                }
                // 两种行集形状（层视图 / 全量搜索）由 args 定，见 `session_panel`。
                let panel = session_panel(candidates, args, query);
                let count = panel.rows.len();
                let filter = args.to_string();
                let mut state = SelectionState::with_max_visible(count, rich_max_visible());
                select_exact_match(&panel.rows, args, &mut state);
                *self = Self::SubCommand {
                    command: cmd.to_string(),
                    filter,
                    rows: panel.rows,
                    state,
                    chrome: panel.chrome,
                };
                return None;
            }
            // Candidates not cached — trigger HTTP fetch.
            // Show command popup while waiting.
            let filter = cmd[1..].to_string();
            let rows = filter_commands(&cache.commands, &filter);
            let count = rows.len();
            *self = Self::Command {
                filter,
                rows,
                state: SelectionState::new(count),
            };
            return Some(PopupAction::FetchSessionList);
        }

        // Check if we're in sub-command mode for local-only candidates (e.g. /copy).
        if is_local_candidate_command(cmd) {
            if let Some(candidates) = cache.get_for_command(cmd) {
                // Hide popup if args exactly match a candidate.
                if !args.is_empty() && is_exact_candidate_match(candidates, args) {
                    *self = Self::None;
                    return None;
                }
                let rows = filter_candidates(candidates, args);
                let count = rows.len();
                let filter = args.to_string();
                // /copy: newest message is the expected default.
                let state = SelectionState::new_selecting_last(count);
                *self = Self::SubCommand {
                    command: cmd.to_string(),
                    filter,
                    rows,
                    state,
                    chrome: SessionChrome::default(),
                };
                return None;
            }
            *self = Self::None;
            return None;
        }

        // Check if we're in sub-command mode for HTTP-fetched candidates.
        if let Some(action) = candidate_request_for(cmd) {
            // Check if candidates are cached.
            if let Some(candidates) = cache.get_for_command(cmd) {
                // must-select 命令精确匹配时保持 popup 打开（Enter = 确认选择）；
                // 非 must-select 命令精确匹配即隐藏。
                if !command::is_must_select_command(cmd)
                    && !args.is_empty()
                    && is_exact_candidate_match(candidates, args)
                {
                    *self = Self::None;
                    return None;
                }
                let rows = filter_candidates(candidates, args);
                let count = rows.len();
                let filter = args.to_string();
                let mut state = SelectionState::new(count);
                select_exact_match(&rows, args, &mut state);
                *self = Self::SubCommand {
                    command: cmd.to_string(),
                    filter,
                    rows,
                    state,
                    chrome: SessionChrome::default(),
                };
                return None;
            }

            // Candidates not cached — show command popup while fetching.
            let filter = cmd[1..].to_string();
            let rows = filter_commands(&cache.commands, &filter);
            let count = rows.len();
            *self = Self::Command {
                filter,
                rows,
                state: SelectionState::new(count),
            };
            return Some(action.clone());
        }

        // Default: command list popup.
        let filter = cmd[1..].to_string(); // strip leading `/`
        // Hide popup if filter exactly matches a command name.
        if !filter.is_empty() {
            let filter_lower = filter.to_lowercase();
            let exact_match = cache
                .commands
                .iter()
                .any(|c| c.name.to_lowercase() == filter_lower)
                || command::is_tui_only_command(&filter);
            if exact_match {
                *self = Self::None;
                return None;
            }
        }
        let rows = filter_commands(&cache.commands, &filter);
        let count = rows.len();
        *self = Self::Command {
            filter,
            rows,
            state: SelectionState::new(count),
        };
        None
    }

    /// Get the popup height (0 if no popup).
    ///
    /// chrome 行（面包屑 / dormant 尾注）算在里面：它们占 popup 的上下各一行，
    /// 排版按这个高度给矩形，渲染侧再从矩形里扣掉。
    pub fn height(&self) -> u16 {
        match self {
            Self::None => 0,
            Self::Command { rows, state, .. } => popup_height(rows, state.max_visible),
            Self::SubCommand {
                rows,
                state,
                chrome,
                ..
            } => popup_height(rows, state.max_visible) + chrome.height(),
        }
    }

    /// Whether a popup is active.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// 会话面板的层视图：`/session`、`/ss` 子命令态 + 参数为空 + **画得出来**。
    ///
    /// 「参数为空 = 层视图」这个行集形状的判定归投影（
    /// [`command::session_panel`](crate::ui::popup::command::session_panel)）；这里只回答
    /// 「这个面板此刻是不是那个形状」。
    ///
    /// 第三个条件是键盘接管的门槛（`docs/dev/tui-input.md` 的可见性口径）：一个高度为 0
    /// 的面板不接管任何键，否则「看起来空闲」的界面按键不响应。
    pub fn session_level_view(&self) -> bool {
        match self {
            Self::SubCommand {
                command, filter, ..
            } => is_session_command(command) && filter.is_empty() && self.height() > 0,
            _ => false,
        }
    }

    /// Whether the popup has any matching items to select from.
    pub fn has_items(&self) -> bool {
        match self {
            Self::None => false,
            Self::Command { rows, .. } | Self::SubCommand { rows, .. } => !rows.is_empty(),
        }
    }

    /// must-select 命令且候选列表为空。
    ///
    /// 此类命令的参数必须来自候选：列表为空时 Enter 需给显式反馈，
    /// MUST NOT 落到自由文本发送（那会构造出未定义请求，如无 provider
    /// 的 model 更新）。
    pub fn is_must_select_empty(&self) -> bool {
        match self {
            Self::SubCommand { command, rows, .. } => {
                rows.is_empty() && command::is_must_select_command(command)
            }
            _ => false,
        }
    }

    /// Move selection up.
    pub fn move_up(&mut self) {
        match self {
            Self::Command { state, .. } | Self::SubCommand { state, .. } => state.move_up(),
            Self::None => {}
        }
    }

    /// Move selection down.
    pub fn move_down(&mut self) {
        match self {
            Self::Command { state, .. } | Self::SubCommand { state, .. } => state.move_down(),
            Self::None => {}
        }
    }

    /// 光标上 / 下移一屏（`Ctrl+U` / `Ctrl+D`）。钳在两端，不环绕——翻页是「跳到视野
    /// 的另一头」，绕着环回到刚才那一头只会让人以为自己没按下去。
    pub fn move_page(&mut self, down: bool) {
        match self {
            Self::Command { state, .. } | Self::SubCommand { state, .. } => {
                let page = state.max_visible;
                let page = page as isize;
                state.move_by(if down { page } else { -page });
            }
            Self::None => {}
        }
    }

    /// Get the selected item's name (for completion).
    pub fn selected_name(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Command { rows, state, .. } => rows.get(state.selected).map(|r| r.name.as_str()),
            Self::SubCommand { rows, state, .. } => {
                rows.get(state.selected).map(|r| r.name.as_str())
            }
        }
    }

    /// 高亮行的 crew 聚合（只有层视图的行有；`None` = 没有选中 / 不是层视图的行）。
    ///
    /// 「能不能进去」= `descendants > 0`：事实来自这次投影（内核聚合，每个有效子贡献
    /// 1 + 自己的后代），调用方不用另算一遍拓扑。
    pub fn selected_crew(&self) -> Option<Crew> {
        match self {
            Self::Command { rows, state, .. } | Self::SubCommand { rows, state, .. } => rows
                .get(state.selected)
                .and_then(|row| row.rich.as_ref())
                .and_then(|rich| rich.level)
                .map(|level| level.crew),
            Self::None => None,
        }
    }

    /// 当前选中行的下标（过界按最后一行算；没有行 = `None`）。
    pub fn selected_index(&self) -> Option<usize> {
        match self {
            Self::None => None,
            Self::Command { rows, state, .. } | Self::SubCommand { rows, state, .. } => {
                (!rows.is_empty()).then(|| state.selected.min(rows.len() - 1))
            }
        }
    }

    /// 把光标放到第 `index` 行（clamp）——行集收窄时用它停在「原来那个位置」附近的行上。
    pub fn select_index(&mut self, index: usize) {
        match self {
            Self::Command { state, .. } | Self::SubCommand { state, .. } => state.select(index),
            Self::None => {}
        }
    }

    /// 把选中位重新对准某一行（按 `name` 精确匹配；不存在则不动）。
    ///
    /// 用于「原地刷新」：行序可能因 pin / 活跃度变化，光标按 id 回到原来
    /// 那一行，而不是留在下标处（那会指到别的会话）。
    pub fn reselect(&mut self, name: &str) -> bool {
        match self {
            Self::None => false,
            Self::Command { rows, state, .. } | Self::SubCommand { rows, state, .. } => {
                match rows.iter().position(|row| row.name == name) {
                    Some(index) => {
                        state.select(index);
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// 当前是否处于 session 候选面板（`/session` / `/ss` 的子命令态）。
    ///
    /// 「打开面板即刷新一次」（#147）靠它判断面板的进入 / 离开：**同一个
    /// 面板身份内只刷新一次**，刷新响应原地换行不再次触发。
    /// 注意等待首帧列表的 `Command` 等待态不算面板本体——那条路径自己
    /// 返回 [`PopupAction::FetchSessionList`]，重复计数会多打一次请求。
    pub fn session_panel_command(&self) -> Option<&str> {
        match self {
            Self::SubCommand { command, .. } if is_session_command(command) => Some(command),
            _ => None,
        }
    }

    /// Build the completion text for the selected item.
    ///
    /// For commands with candidates: returns command + space (e.g. "/model ").
    /// For commands without: returns the command name.
    /// For sub-commands: returns the full input (e.g. "/model gpt-4o").
    pub fn completion_text(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Command { rows, state, .. } => {
                let row = rows.get(state.selected)?;
                let has_candidates = candidate_request_for(&row.name).is_some()
                    || is_local_candidate_command(&row.name);
                if has_candidates {
                    Some(format!("{} ", row.name))
                } else {
                    Some(row.name.to_string())
                }
            }
            Self::SubCommand {
                command,
                rows,
                state,
                ..
            } => {
                let row = rows.get(state.selected)?;
                Some(format!("{command} {}", row.name))
            }
        }
    }

    /// Whether the selected item should be submitted immediately.
    ///
    /// Commands without candidates are submitted on Enter.
    /// Commands with candidates or sub-commands need more input.
    pub fn should_submit(&self) -> bool {
        match self {
            Self::None => false,
            Self::Command { rows, state, .. } => {
                if let Some(row) = rows.get(state.selected) {
                    candidate_request_for(&row.name).is_none()
                        && !is_local_candidate_command(&row.name)
                } else {
                    false
                }
            }
            Self::SubCommand { rows, state, .. } => {
                // Only submit if a valid candidate is selected.
                rows.get(state.selected).is_some()
            }
        }
    }

    /// Access the popup for rendering.
    ///
    /// 第四个元素是 chrome 行：`None` = 非候选面板（命令列表 / 没有面板）；子命令面板一律
    /// `Some`，其中**非层视图的**（`/fork` / `/agents` / `/copy`）是空 chrome（高度 0）。
    pub fn render_data(
        &self,
    ) -> Option<(
        &[SelectionRow],
        &SelectionState,
        &str,
        Option<&SessionChrome>,
    )> {
        match self {
            Self::None => None,
            Self::Command {
                rows,
                state,
                filter,
            } => Some((rows, state, filter, None)),
            Self::SubCommand {
                rows,
                state,
                filter,
                chrome,
                ..
            } => Some((rows, state, filter, Some(chrome))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CommandInfo;

    /// Build a SessionCandidate for tests.
    fn sess(id: &str, title: &str) -> command::SessionCandidate {
        command::SessionCandidate {
            id: id.into(),
            parent: None,
            title: title.into(),
            workspace: "/tmp/ws".into(),
            status: "idle".into(),
            last_interaction: "2025-07-22T21:41:00".into(),
            pinned: false,
            pin_added_at: None,
        }
    }

    fn sample_cache() -> CandidateCache {
        CandidateCache {
            commands: vec![
                CommandInfo {
                    name: "help".into(),
                    aliases: vec![],
                    description: "Show commands".into(),
                    params: String::new(),
                },
                CommandInfo {
                    name: "model".into(),
                    aliases: vec![],
                    description: "Switch model".into(),
                    params: String::new(),
                },
            ],
            ..CandidateCache::default()
        }
    }

    #[test]
    fn test_active_popup_none_by_default() {
        let popup = ActivePopup::default();
        assert!(!popup.is_active());
        assert_eq!(popup.height(), 0);
    }

    #[test]
    fn test_active_popup_command_mode() {
        let mut popup = ActivePopup::default();
        let cache = sample_cache();
        popup.update_from_input("/mo", &cache, SessionQuery::default());
        assert!(popup.is_active());
        assert!(popup.height() > 0);
    }

    #[test]
    fn test_active_popup_sub_command_mode() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        cache.branches = vec![
            ("uuid-1".into(), "first".into()),
            ("uuid-2".into(), "second".into()),
        ];
        popup.update_from_input("/fork ", &cache, SessionQuery::default());
        assert!(popup.is_active());
        assert!(matches!(popup, ActivePopup::SubCommand { .. }));
    }

    #[test]
    fn test_active_popup_completion() {
        let mut popup = ActivePopup::default();
        let cache = sample_cache();
        popup.update_from_input("/he", &cache, SessionQuery::default());
        let completion = popup.completion_text();
        assert!(completion.is_some());
        assert_eq!(completion.unwrap(), "/help");
    }

    #[test]
    fn test_active_popup_sub_completion() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        cache.branches = vec![("uuid-1".into(), "first".into())];
        popup.update_from_input("/fork uu", &cache, SessionQuery::default());
        let completion = popup.completion_text();
        assert!(completion.is_some());
        assert_eq!(completion.unwrap(), "/fork uuid-1");
    }

    #[test]
    fn test_active_popup_navigation() {
        let mut popup = ActivePopup::default();
        let cache = sample_cache();
        popup.update_from_input("/", &cache, SessionQuery::default());
        popup.move_down();
        let name1 = popup.selected_name().map(String::from);
        popup.move_down();
        let name2 = popup.selected_name().map(String::from);
        assert_ne!(name1, name2);
    }

    #[test]
    fn test_active_popup_should_submit() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        // /help exact match → popup hidden (None).
        popup.update_from_input("/help", &cache, SessionQuery::default());
        assert!(!popup.is_active());
        // /hel partial match → popup shows, should submit (no candidates).
        popup.update_from_input("/hel", &cache, SessionQuery::default());
        assert!(popup.is_active());
        assert!(popup.should_submit());
        // /for partial match → popup shows, should NOT submit (has candidates).
        cache.branches = vec![("uuid-1".into(), "first".into())];
        popup.update_from_input("/for", &cache, SessionQuery::default());
        assert!(popup.is_active());
        assert!(!popup.should_submit());
    }

    #[test]
    fn test_must_select_exact_match_keeps_popup() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        // 非候选命令精确匹配 → 隐藏 popup（原行为不变）。
        popup.update_from_input("/clear", &cache, SessionQuery::default());
        assert!(!popup.is_active());
        // must-select 命令精确匹配 → popup 保持打开，选中位跟随精确行
        //（Enter = 确认选择，而非自由文本发送）。
        cache.branches = vec![
            ("uuid-1".into(), "first".into()),
            ("uuid-2".into(), "second".into()),
        ];
        popup.update_from_input("/fork uuid-1", &cache, SessionQuery::default());
        assert!(popup.is_active());
        assert!(popup.should_submit());
        if let ActivePopup::SubCommand { rows, state, .. } = &popup {
            assert_eq!(rows[state.selected].name, "uuid-1");
        } else {
            panic!("expected SubCommand popup");
        }
        // 部分匹配同样显示 popup。
        popup.update_from_input("/fork uuid", &cache, SessionQuery::default());
        assert!(popup.is_active());
    }

    #[test]
    fn test_must_select_empty_detection() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        cache.agents = vec![("coder".into(), String::new())];
        // 无匹配候选 → must-select 空（Enter 需给显式反馈，不落自由发送）。
        popup.update_from_input("/agents nonexistent", &cache, SessionQuery::default());
        assert!(popup.is_must_select_empty());
        // 有匹配候选 → 非空。
        popup.update_from_input("/agents cod", &cache, SessionQuery::default());
        assert!(!popup.is_must_select_empty());
        // 非 must-select 命令的空 popup → false。
        popup.update_from_input("/zzz", &cache, SessionQuery::default());
        assert!(!popup.is_must_select_empty());
    }

    #[test]
    fn test_active_popup_sub_should_submit_requires_selection() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        cache.branches = vec![("uuid-1".into(), "first".into())];
        popup.update_from_input("/fork ", &cache, SessionQuery::default());
        // Has a selection → should submit.
        assert!(popup.should_submit());
    }

    #[test]
    fn test_session_empty_cache_returns_fetch_action() {
        use command::PopupAction;

        let mut popup = ActivePopup::default();
        let cache = CandidateCache::default(); // sessions empty
        let action = popup.update_from_input("/session ", &cache, SessionQuery::default());
        assert!(matches!(action, Some(PopupAction::FetchSessionList)));
    }

    #[test]
    fn test_ss_empty_cache_returns_fetch_action() {
        use command::PopupAction;

        let mut popup = ActivePopup::default();
        let cache = CandidateCache::default();
        let action = popup.update_from_input("/ss ", &cache, SessionQuery::default());
        assert!(matches!(action, Some(PopupAction::FetchSessionList)));
    }

    #[test]
    fn test_session_cached_shows_popup_no_action() {
        let mut popup = ActivePopup::default();
        let cache = CandidateCache {
            sessions: vec![sess("sess-1", "My Session"), sess("sess-2", "Other")],
            sessions_fetched: true,
            ..CandidateCache::default()
        };
        let action = popup.update_from_input("/session ", &cache, SessionQuery::default());
        assert!(action.is_none()); // No fetch needed
        assert!(popup.is_active());
        assert!(matches!(popup, ActivePopup::SubCommand { .. }));
    }

    #[test]
    fn test_session_exact_match_keeps_popup_selects_row() {
        let mut popup = ActivePopup::default();
        let cache = CandidateCache {
            sessions: vec![sess("sess-1", "My Session"), sess("sess-2", "Other")],
            sessions_fetched: true,
            ..CandidateCache::default()
        };
        let action = popup.update_from_input("/session sess-1", &cache, SessionQuery::default());
        assert!(action.is_none());
        // must-select：精确匹配不隐藏 popup，选中位跟随该行。
        assert!(popup.is_active());
        if let ActivePopup::SubCommand { rows, state, .. } = &popup {
            assert_eq!(rows[state.selected].name, "sess-1");
        } else {
            panic!("expected SubCommand popup");
        }
    }

    #[test]
    fn test_reselect_re_anchors_by_name_and_leaves_a_missing_one_alone() {
        let mut popup = ActivePopup::default();
        let cache = CandidateCache {
            sessions: vec![sess("sess-1", "My Session"), sess("sess-2", "Other")],
            sessions_fetched: true,
            ..CandidateCache::default()
        };
        popup.update_from_input("/ss", &cache, SessionQuery::default());
        assert_eq!(popup.selected_name(), Some("sess-1"));

        // 命中锚点：光标跟到那一行（原地刷新后行序变了也不跳走）。
        assert!(popup.reselect("sess-2"));
        assert_eq!(popup.selected_name(), Some("sess-2"));

        // 锚点不在列表里（会话被删 / 过滤掉了）：光标**不动**，返回 false。
        assert!(!popup.reselect("gone"));
        assert_eq!(popup.selected_name(), Some("sess-2"));

        // 没有面板时什么都不做。
        let mut none = ActivePopup::None;
        assert!(!none.reselect("sess-1"));
        assert!(matches!(none, ActivePopup::None));
    }

    #[test]
    fn test_session_popup_rows_are_rich() {
        let mut popup = ActivePopup::default();
        let cache = CandidateCache {
            sessions: vec![sess("sess-1", "My Session")],
            sessions_fetched: true,
            ..CandidateCache::default()
        };
        popup.update_from_input("/session ", &cache, SessionQuery::default());
        if let ActivePopup::SubCommand { rows, state, .. } = &popup {
            assert_eq!(rows.len(), 1);
            assert!(rows[0].rich.is_some());
            // Rich popup uses the smaller visible-item window.
            assert_eq!(state.max_visible, rich_max_visible());
        } else {
            panic!("expected SubCommand popup");
        }
    }

    #[test]
    fn test_fork_triggers_fetch_branches() {
        use command::PopupAction;

        let mut popup = ActivePopup::default();
        let cache = CandidateCache::default();
        let action = popup.update_from_input("/fork ", &cache, SessionQuery::default());
        assert!(matches!(action, Some(PopupAction::FetchBranches)));
    }
}
