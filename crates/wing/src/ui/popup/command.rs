//! Slash command definitions, filtering, and sub-command candidate management.
//!
//! Dynamic command list from gateway + static TUI-only fallbacks.
//! Handles:
//! - Input text → filter extraction
//! - Prefix matching with sort (exact > prefix)
//! - Sub-command candidates (model list, session list, etc.)

use std::collections::HashSet;
use std::sync::LazyLock;

use super::selection::DormantFootnote;
use super::selection::LevelDecor;
use super::selection::RichSessionRow;
use super::selection::SelectionRow;
use super::selection::SessionChrome;
use super::selection::format_last_active;
use super::selection::plain_row;
use crate::protocol::CommandInfo;
use crate::protocol::SessionStatus;
use crate::shared::session_tree::Forest;
use crate::shared::session_tree::TreeRow;

/// Action to take when popup candidates need to be fetched.
///
/// Each variant maps to a specific HTTP API call or intent.
#[derive(Debug, Clone)]
pub enum PopupAction {
    /// Fetch branch targets via HTTP API for popup candidates.
    FetchBranches,
    /// Fetch agent template list via HTTP API for popup candidates.
    FetchAgents,
    /// Fetch session list via HTTP API (Phase 3c).
    FetchSessionList,
}

/// Commands that have sub-command candidates fetched via HTTP.
/// Maps command name to the corresponding PopupAction.
const CANDIDATE_COMMANDS: &[(&str, PopupAction)] = &[
    ("/fork", PopupAction::FetchBranches),
    ("/rewind", PopupAction::FetchBranches),
    ("/agents", PopupAction::FetchAgents),
];

/// Local-only candidate commands — candidates populated by App, no fetch needed.
const LOCAL_CANDIDATE_COMMANDS: &[&str] = &["/copy"];

/// Commands whose argument MUST come from candidate selection.
///
/// For these commands Enter means "confirm the highlighted candidate", never
/// "send free text": the popup stays open on exact match (Tab fills the input
/// without dismissing it), and submission is only constructed from a selected
/// candidate. This structurally eliminates undefined resolution — e.g.
/// `/fork <uuid>` where the uuid is absent from the fetched list.
const MUST_SELECT_COMMANDS: &[&str] = &["/fork", "/rewind", "/agents", "/session", "/ss"];

/// TUI-only commands (not served by gateway) — always appended as fallback.
///
/// These are commands handled entirely by the TUI (via `try_http_command()` or
/// local logic). The full `CommandInfo` structure allows the popup to display
/// aliases and parameter hints.
static TUI_ONLY_COMMANDS: LazyLock<Vec<CommandInfo>> = LazyLock::new(|| {
    vec![
        CommandInfo {
            name: "clear".into(),
            aliases: vec![],
            description: "Clear chat view".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "copy".into(),
            aliases: vec![],
            description: "Copy assistant message".into(),
            params: "[N]".into(),
        },
        CommandInfo {
            name: "tips".into(),
            aliases: vec![],
            description: "List every startup tip".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "new".into(),
            aliases: vec![],
            description: "Create new session".into(),
            params: "[name]".into(),
        },
        CommandInfo {
            name: "session".into(),
            aliases: vec!["ss".into()],
            description: "Switch or list sessions".into(),
            params: "[session_id]".into(),
        },
        CommandInfo {
            name: "fork".into(),
            aliases: vec![],
            description: "Fork session at message".into(),
            params: "<uuid>".into(),
        },
        CommandInfo {
            name: "model".into(),
            aliases: vec!["m".into()],
            description: "Select a model".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "agents".into(),
            aliases: vec![],
            description: "Switch or show agent template".into(),
            params: "[name]".into(),
        },
        CommandInfo {
            name: "title".into(),
            aliases: vec![],
            description: "Show or set session title".into(),
            params: "[name]".into(),
        },
        CommandInfo {
            name: "workdir".into(),
            aliases: vec![],
            description: "Switch working directory".into(),
            params: "<path>".into(),
        },
        CommandInfo {
            name: "think".into(),
            aliases: vec!["t".into()],
            description: "Toggle thinking / set effort".into(),
            params: "on|off|low|medium|high|xhigh|max".into(),
        },
        CommandInfo {
            name: "yolo".into(),
            aliases: vec![],
            description: "Toggle YOLO mode".into(),
            params: "on|off".into(),
        },
        CommandInfo {
            name: "compact".into(),
            aliases: vec![],
            description: "Compress session context".into(),
            params: "[focus]".into(),
        },
        CommandInfo {
            name: "context".into(),
            aliases: vec![],
            description: "Show context stats and system prompt".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "skills".into(),
            aliases: vec![],
            description: "Show loaded skills".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "rewind".into(),
            aliases: vec![],
            description: "Rewind to a specific message".into(),
            params: "<uuid>".into(),
        },
        CommandInfo {
            name: "reload".into(),
            aliases: vec![],
            description: "Reload config, hooks, skills".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "settings".into(),
            aliases: vec!["config".into(), "set".into()],
            description: "Edit gateway & interface settings".into(),
            params: String::new(),
        },
    ]
});

/// The bare names of the TUI-only fallback table (tests keep it in step with
/// the command router — see `app::tests::commands`).
#[cfg(test)]
pub(crate) fn tui_only_names() -> Vec<&'static str> {
    TUI_ONLY_COMMANDS.iter().map(|c| c.name.as_str()).collect()
}

/// Check if a bare name (without `/`) matches a TUI-only command (case-insensitive).
pub fn is_tui_only_command(bare_name: &str) -> bool {
    let lower = bare_name.to_lowercase();
    TUI_ONLY_COMMANDS.iter().any(|cmd| {
        cmd.name.to_lowercase() == lower || cmd.aliases.iter().any(|a| a.to_lowercase() == lower)
    })
}

/// Check if a command name has HTTP-fetched sub-command candidates.
pub fn candidate_request_for(name: &str) -> Option<&'static PopupAction> {
    CANDIDATE_COMMANDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, action)| action)
}

/// Check if a command name has locally-populated sub-command candidates.
pub fn is_local_candidate_command(name: &str) -> bool {
    LOCAL_CANDIDATE_COMMANDS.contains(&name)
}

/// Check if a command requires its argument to be selected from candidates.
pub fn is_must_select_command(name: &str) -> bool {
    MUST_SELECT_COMMANDS.contains(&name)
}

/// Check if a command is a session command (`/session` or `/ss`).
/// These are handled via HTTP API instead of WS silent requests.
pub fn is_session_command(name: &str) -> bool {
    name == "/session" || name == "/ss"
}

/// Extract the command part (first word) from input starting with `/`.
///
/// Returns `("/model", "gpt-4o")` for `"/model gpt-4o"`.
pub fn parse_slash_input(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    match trimmed.find(' ') {
        Some(pos) => {
            let cmd = &trimmed[..pos];
            let args = trimmed[pos + 1..].trim_start();
            Some((cmd, args))
        }
        None => Some((trimmed, "")),
    }
}

/// Filter and sort commands by the given filter string.
///
/// `commands` is the dynamic list from gateway (may be empty if not yet fetched).
/// TUI-only commands are always appended.
/// Sort order: exact match > prefix match.
pub fn filter_commands(commands: &[CommandInfo], filter: &str) -> Vec<SelectionRow> {
    let filter_lower = filter.to_lowercase();

    let mut exact = Vec::new();
    let mut prefix = Vec::new();

    // Helper to try adding a command row.
    let mut try_add = |name: &str, desc: &str| {
        let name_stripped = &name[1..]; // strip leading `/`
        let name_lower = name_stripped.to_lowercase();

        if filter_lower.is_empty() {
            prefix.push(plain_row(name, desc));
        } else if name_lower == filter_lower {
            exact.push(plain_row(name, desc));
        } else if name_lower.starts_with(&filter_lower) {
            prefix.push(plain_row(name, desc));
        }
    };

    // Dynamic commands from gateway.
    for cmd in commands {
        let full_name = format!("/{}", cmd.name);
        try_add(&full_name, &cmd.description);
    }

    // TUI-only fallbacks (skip if already provided by gateway).
    let gateway_names: std::collections::HashSet<&str> =
        commands.iter().map(|c| c.name.as_str()).collect();
    for cmd in TUI_ONLY_COMMANDS.iter() {
        if !gateway_names.contains(cmd.name.as_str()) {
            let full_name = format!("/{}", cmd.name);
            let desc = if cmd.params.is_empty() {
                cmd.description.clone()
            } else {
                format!("{} {}", cmd.description, cmd.params)
            };
            try_add(&full_name, &desc);
        }
    }

    exact.extend(prefix);
    exact.sort_by_key(|a| a.name.to_lowercase());
    exact
}

/// Filter sub-command candidates by the given args string.
///
/// Each candidate is `(id, description)`. Matching is done on `id`.
/// `name` in the resulting `SelectionRow` is the `id` (used for completion),
/// `description` is the display label.
pub fn filter_candidates(candidates: &[(String, String)], args: &str) -> Vec<SelectionRow> {
    let args_lower = args.to_lowercase();

    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();

    for (id, desc) in candidates {
        let id_lower = id.to_lowercase();
        let row = || plain_row(id.clone(), desc.clone());
        if args.is_empty() {
            prefix.push(row());
        } else if id_lower == args_lower {
            exact.push(row());
        } else if id_lower.starts_with(&args_lower) {
            prefix.push(row());
        } else if id_lower.contains(&args_lower) {
            contains.push(row());
        }
    }

    exact.extend(prefix);
    exact.extend(contains);
    exact
}

/// 一条会话候选 → 一行双行富行。搜索态与层视图只差两件事：LAST 列取哪个时间、
/// 有没有层级装饰。
fn rich_session_row(
    c: &SessionCandidate,
    last_active: String,
    level: Option<LevelDecor>,
) -> SelectionRow {
    SelectionRow {
        name: c.id.clone(),
        description: String::new(),
        rich: Some(RichSessionRow {
            // Display-only surface: a value this build does not know
            // (version mismatch on the list endpoint) shows as inactive.
            status: SessionStatus::parse(&c.status).unwrap_or(SessionStatus::Inactive),
            last_active,
            title: c.title.clone(),
            workspace: c.workspace.clone(),
            pinned: c.pinned,
            level,
        }),
    }
}

/// **搜索态**的行集（`args` 非空）：与树形之前的搜索逐字一致——跨层、含 dormant、
/// 不带徽章 / 标记（命中集合与树不同构，层级装饰在那个状态下没有意义）。
///
/// Matching is done on id, title, and workspace (case-insensitive; workspace is
/// search-only — it drives no ordering). The order of `candidates` is preserved
/// within each match tier (exact > prefix > contains), so the tree order the
/// candidates were built in survives filtering.
fn search_session_rows(candidates: &[SessionCandidate], args: &str) -> Vec<SelectionRow> {
    debug_assert!(!args.is_empty(), "空参数的会话面板走层视图");
    let args_lower = args.to_lowercase();

    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();

    for c in candidates {
        let id_lower = c.id.to_lowercase();
        let title_lower = c.title.to_lowercase();
        let ws_lower = c.workspace.to_lowercase();
        let row = || rich_session_row(c, format_last_active(&c.last_interaction), None);
        if id_lower == args_lower {
            exact.push(row());
        } else if id_lower.starts_with(&args_lower) || title_lower.starts_with(&args_lower) {
            prefix.push(row());
        } else if id_lower.contains(&args_lower)
            || title_lower.contains(&args_lower)
            || ws_lower.contains(&args_lower)
        {
            contains.push(row());
        }
    }

    exact.extend(prefix);
    exact.extend(contains);
    exact
}

/// 会话面板投影的输入：行集是这三件事的**纯函数**（改它们只换投影，不换数据、
/// 不重取）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionQuery<'a> {
    /// 当前层：`None` = 根层（任务层），`Some(id)` = 该会话的直接子那一层。
    pub scope: Option<&'a str>,
    /// 当前层是否把 dormant（不在内存里的）直接子也列出来。
    pub include_dormant: bool,
    /// 当前会话 id —— 层视图里它的祖先链（含自己）带「你在这里」标记。
    pub current: Option<&'a str>,
}

/// 一次投影的产物：候选行 + chrome 行（后者不是候选行）。
#[derive(Debug, Default)]
pub struct SessionPanel {
    pub rows: Vec<SelectionRow>,
    pub chrome: SessionChrome,
}

/// 会话面板的行集 —— **两种形状**，由参数是否为空决定（见
/// docs/dev/session-lineage.md §4.1）：
///
/// - 参数为空 → **层视图**：`query.scope` 那一层的直接子（存活闭包生效），带 crew
///   徽章与当前会话标记，面包屑 / dormant 尾注以 chrome 行给出；
/// - 参数非空 → **全量搜索**：与树形之前的搜索逐字一致（exact > prefix > contains，
///   跨层、含 dormant、无徽章、无标记、无 chrome）。
pub fn session_panel(
    candidates: &[SessionCandidate],
    args: &str,
    query: SessionQuery<'_>,
) -> SessionPanel {
    if args.is_empty() {
        return level_panel(candidates, query);
    }
    SessionPanel {
        rows: search_session_rows(candidates, args),
        chrome: SessionChrome::default(),
    }
}

/// 层视图：一次一层的投影，全部事实来自内核（`shared::session_tree`）。
///
/// 森林在这里重建一次（候选 → `TreeRow`）：层投影、存活闭包、crew 聚合与祖先链都是
/// `Forest` 的只读视图，自己按 `parent` 过滤会漏掉闭包（dormant 父因活的子而保留）。
/// [`tree_rows`] 是**保序保长**的一一映射，故森林的下标空间 ≡ `candidates` 的下标。
fn level_panel(candidates: &[SessionCandidate], query: SessionQuery<'_>) -> SessionPanel {
    let forest = Forest::build(&tree_rows(candidates));

    // `include_dormant` 打开时内核的 `dormant_children` 恒为 0（一行都没收起），
    // 而尾注要说的正是「收起了几条」——所以另取一次关闭态。
    let hidden = forest.level(query.scope, false).dormant_children;
    let level = forest.level(query.scope, query.include_dormant);

    // LAST 列 = 子树最大活跃（内核聚合）。行 1 要的是与今天同格式的文本，所以取内核指出
    // 的**来源行**原文交给同一个格式化函数：把微秒格式化回来会与 `format_last_active` 的
    // 口径分叉（RFC3339 手改值会显示成 UTC 时钟），按微秒反查原文的键又是有损的（同一
    // 时刻的两种书写会互相串行）。
    let chain = current_chain(&forest, candidates, query.current);

    let rows = level
        .rows
        .iter()
        .map(|&index| {
            let c = &candidates[index];
            let crew = forest.crew(index);
            // 越界不会发生：`index` 来自上面那次 `level` 投影。
            let source = forest.activity_of(index).unwrap_or(index);
            let last_active = format_last_active(&candidates[source].last_interaction);
            rich_session_row(
                c,
                last_active,
                Some(LevelDecor {
                    crew,
                    current: chain.contains(&index),
                }),
            )
        })
        .collect();

    SessionPanel {
        rows,
        chrome: level_chrome(candidates, query, hidden),
    }
}

/// 当前会话在森林里的祖先链（含自己）。链是一条路径 ⇒ **每层至多一个**标记行。
///
/// 当前会话不在这一批候选里（刚建还没进列表 / 已被逐出）→ 空集：不猜。
fn current_chain(
    forest: &Forest,
    candidates: &[SessionCandidate],
    current: Option<&str>,
) -> HashSet<usize> {
    let mut chain = HashSet::new();
    let Some(current) = current else {
        return chain;
    };
    let Some(start) = candidates.iter().position(|c| c.id == current) else {
        return chain;
    };
    let mut cursor = Some(start);
    while let Some(index) = cursor {
        if !chain.insert(index) {
            // 有效父链已由内核断环；这里只保证「不挂死」而不是「画得对」。
            break;
        }
        cursor = forest.parent_of(index);
    }
    chain
}

/// 层视图的 chrome 行：面包屑（层 ≥ 1 才有）+ dormant 尾注（有被收起的才有）。
fn level_chrome(
    candidates: &[SessionCandidate],
    query: SessionQuery<'_>,
    hidden: usize,
) -> SessionChrome {
    let breadcrumb = query.scope.map(|scope| {
        candidates
            .iter()
            .find(|c| c.id == scope)
            .map(|c| {
                if c.title.is_empty() {
                    c.id.clone()
                } else {
                    c.title.clone()
                }
            })
            // scope 不在候选里（payload 刚变、App 还没回落）：名字就用 id。
            .unwrap_or_else(|| scope.to_string())
    });
    let footnote = (hidden > 0).then_some(DormantFootnote {
        hidden,
        shown: query.include_dormant,
    });
    SessionChrome {
        breadcrumb,
        footnote,
    }
}

/// 候选 → 内核的输入切片（层投影的唯一入口）。保序保长：森林吐出的下标即候选下标。
fn tree_rows(candidates: &[SessionCandidate]) -> Vec<TreeRow<'_>> {
    candidates
        .iter()
        .map(|c| TreeRow {
            id: &c.id,
            parent: c.parent.as_deref(),
            pinned: c.pinned,
            pin_added_at: c.pin_added_at.as_deref(),
            // 空串 = 没有时间（内核解析不了、`format_last_active` 给空串，同一条降级）。
            last_interaction: Some(c.last_interaction.as_str()),
            status: SessionStatus::parse(&c.status),
        })
        .collect()
}

/// Returns `true` if `args` exactly matches a session candidate id (case-insensitive).
pub fn is_exact_session_match(candidates: &[SessionCandidate], args: &str) -> bool {
    let args_lower = args.to_lowercase();
    candidates.iter().any(|c| c.id.to_lowercase() == args_lower)
}

/// A session candidate for the `/session`、`/ss` popup.
///
/// Carries everything needed for the two-line render (status icon + workspace +
/// last active time on line 1, title on line 2) plus the `id` used as the
/// completion value. `workspace` doubles as a search field.
///
/// 候选取自**全部**会话，但抓取序只决定搜索态的行序：层视图每次投影都用下面这些字段
/// **重建森林**（内核不看输入顺序），所以 `parent` / `pinned` / `pin_added_at` /
/// `last_interaction` / `status` 都是**数据**，不是装饰——少任何一个，层内的行序与
/// 子树活跃都会静默偏离[排序契约](crate::shared::session_tree)。
#[derive(Debug, Clone)]
pub struct SessionCandidate {
    /// Session id — inserted on completion (not displayed verbatim).
    pub id: String,
    /// 创建者会话 id（`parent_session_id`；根 = `None`）。
    pub parent: Option<String>,
    /// Session title (line 2).
    pub title: String,
    /// Session workspace — displayed on line 1 and searched by `args`.
    pub workspace: String,
    /// Runtime status string from the backend (inactive|idle|working|waiting)：
    /// 行 1 的状态图标，也是层视图重建森林时的输入（注意力 / crew 计数 / 存活）。
    pub status: String,
    /// Last interaction timestamp (ISO 8601)：**这一行自己的**最后交互，也是重建森林时的
    /// 定序输入。层视图里行 1 显示的可能是子树里**来源行**的原文（见 `level_panel`），
    /// 但本字段始终是它自己的值。
    pub last_interaction: String,
    /// 是否被 pin（`pin` 标签；前端约定，见 [`crate::shared::pinning`]）：行末的星标
    /// + 重建森林时的第一把排序键。
    pub pinned: bool,
    /// pin 时间（`tag_meta.pin.added_at`）：层投影重建森林时的排序键之一。
    pub pin_added_at: Option<String>,
}

/// Cached sub-command candidate data and dynamic command list.
///
/// Most candidate lists use `(id, description)` tuples. The `id` is the value
/// sent to the gateway on completion; `description` is display-only. Sessions
/// use the richer [`SessionCandidate`] to drive the two-line popup.
#[derive(Debug, Clone, Default)]
pub struct CandidateCache {
    /// Dynamic command list from HTTP GET /api/commands.
    pub commands: Vec<CommandInfo>,
    /// Session list from HTTP GET /api/session/list (rich, two-line render).
    pub sessions: Vec<SessionCandidate>,
    /// 会话列表**是否已经抓过**——空列表是合法结果（"还没抓"与"抓到了 0 条"
    /// 必须区分开：混为一谈会让面板在空列表上把"等待首帧"分支反复走一遍，
    /// 每次刷新响应都再发一次请求）。
    pub sessions_fetched: bool,
    /// Branch targets from HTTP GET /api/session/branches or WS BranchTargetsEvent.
    pub branches: Vec<(String, String)>,
    /// Agent list from HTTP GET /api/agents. Description is empty.
    pub agents: Vec<(String, String)>,
    /// Copy candidates (local-only). `(1-based index, first-line preview)`.
    pub copies: Vec<(String, String)>,
}

impl CandidateCache {
    pub fn clear(&mut self) {
        // commands 不清空——来自 gateway 的全局命令列表，不随 session 变化
        self.invalidate_sessions();
        self.branches.clear();
        self.agents.clear();
        self.copies.clear();
    }

    /// 会话候选失效：下次打开面板必须重新抓取（会话切换等动作调用）。
    pub fn invalidate_sessions(&mut self) {
        self.sessions.clear();
        self.sessions_fetched = false;
    }

    /// Whether session candidates are cached (for `/ss`、`/session`).
    pub fn has_sessions(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Get candidates for a given command name (non-session commands).
    pub fn get_for_command(&self, cmd_name: &str) -> Option<&[(String, String)]> {
        match cmd_name {
            "/fork" | "/rewind" if !self.branches.is_empty() => Some(&self.branches),
            "/agents" if !self.agents.is_empty() => Some(&self.agents),
            "/copy" if !self.copies.is_empty() => Some(&self.copies),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_commands() -> Vec<CommandInfo> {
        vec![
            CommandInfo {
                name: "help".into(),
                aliases: vec!["h".into(), "?".into()],
                description: "Show available commands".into(),
                params: String::new(),
            },
            CommandInfo {
                name: "model".into(),
                aliases: vec!["m".into()],
                description: "Switch model".into(),
                params: String::new(),
            },
            CommandInfo {
                name: "compact".into(),
                aliases: vec![],
                description: "Trigger context compaction".into(),
                params: "[focus]".into(),
            },
        ]
    }

    #[test]
    fn test_parse_slash_input() {
        assert_eq!(parse_slash_input("/model"), Some(("/model", "")));
        assert_eq!(parse_slash_input("/model gpt"), Some(("/model", "gpt")));
        assert_eq!(parse_slash_input("/"), Some(("/", "")));
        assert_eq!(parse_slash_input("hello"), None);
        assert_eq!(parse_slash_input(""), None);
    }

    #[test]
    fn test_filter_commands_with_cache() {
        let cmds = sample_commands();
        let rows = filter_commands(&cmds, "");
        // Dynamic commands (primary names only, no aliases) + TUI-only fallbacks.
        assert!(rows.iter().any(|r| r.name == "/help"));
        assert!(!rows.iter().any(|r| r.name == "/h")); // alias NOT shown
        assert!(rows.iter().any(|r| r.name == "/model"));
        assert!(rows.iter().any(|r| r.name == "/clear")); // TUI-only
        assert!(rows.iter().any(|r| r.name == "/tips")); // TUI-only
    }

    #[test]
    fn test_filter_commands_empty_cache() {
        // No gateway commands yet — only TUI-only fallbacks.
        let rows = filter_commands(&[], "");
        assert!(rows.iter().any(|r| r.name == "/clear"));
    }

    #[test]
    fn test_filter_commands_prefix() {
        let cmds = sample_commands();
        let rows = filter_commands(&cmds, "mo");
        // Only /model matches "mo" prefix — alias /m is too short.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "/model");
    }

    #[test]
    fn test_filter_commands_exact() {
        let cmds = sample_commands();
        let rows = filter_commands(&cmds, "help");
        assert!(!rows.is_empty());
        assert_eq!(rows[0].name, "/help");
    }

    #[test]
    fn test_filter_commands_no_match() {
        let cmds = sample_commands();
        let rows = filter_commands(&cmds, "zzz");
        assert!(rows.is_empty());
    }

    #[test]
    fn test_filter_candidates() {
        let candidates = vec![
            ("gpt-4o".into(), String::new()),
            ("gpt-4".into(), String::new()),
            ("claude-3-opus".into(), String::new()),
        ];
        let rows = filter_candidates(&candidates, "gpt");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "gpt-4o");
    }

    #[test]
    fn test_filter_candidates_empty() {
        let candidates = vec![("a".into(), String::new()), ("b".into(), String::new())];
        let rows = filter_candidates(&candidates, "");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_filter_candidates_with_description() {
        let candidates = vec![
            ("abc123".into(), "my-session".into()),
            ("def456".into(), "other".into()),
        ];
        let rows = filter_candidates(&candidates, "abc");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "abc123"); // id used for completion
        assert_eq!(rows[0].description, "my-session"); // display only
    }

    #[test]
    fn test_candidate_cache() {
        let mut cache = CandidateCache::default();
        assert!(cache.get_for_command("/rewind").is_none());
        cache.branches = vec![("uuid-1".into(), "preview".into())];
        assert_eq!(cache.get_for_command("/rewind").unwrap().len(), 1);
        cache.clear();
        assert!(cache.get_for_command("/rewind").is_none());
    }

    #[test]
    fn test_candidate_request_for() {
        assert!(matches!(
            candidate_request_for("/fork"),
            Some(PopupAction::FetchBranches)
        ));
        assert!(matches!(
            candidate_request_for("/rewind"),
            Some(PopupAction::FetchBranches)
        ));
        assert!(matches!(
            candidate_request_for("/agents"),
            Some(PopupAction::FetchAgents)
        ));
        // /model no longer has popup candidates — it opens the model panel.
        assert!(candidate_request_for("/model").is_none());
        // /ss and /session removed from CANDIDATE_COMMANDS (Phase 3c: HTTP-fetched).
        assert!(candidate_request_for("/ss").is_none());
        assert!(candidate_request_for("/session").is_none());
        assert!(candidate_request_for("/help").is_none());
        assert!(candidate_request_for("/nonexistent").is_none());
        // /copy is a local candidate command, not in CANDIDATE_COMMANDS.
        assert!(candidate_request_for("/copy").is_none());
    }

    #[test]
    fn test_is_local_candidate_command() {
        assert!(is_local_candidate_command("/copy"));
        assert!(!is_local_candidate_command("/model"));
        assert!(!is_local_candidate_command("/help"));
    }

    #[test]
    fn test_is_session_command() {
        assert!(is_session_command("/session"));
        assert!(is_session_command("/ss"));
        assert!(!is_session_command("/model"));
        assert!(!is_session_command("/fork"));
    }

    /// A session candidate as the tree-ordered fetch builds it.
    fn session(id: &str, title: &str, workspace: &str, status: &str) -> SessionCandidate {
        SessionCandidate {
            id: id.into(),
            title: title.into(),
            workspace: workspace.into(),
            status: status.into(),
            last_interaction: "2025-07-22T21:41:00".into(),
            pinned: false,
            parent: None,
            pin_added_at: None,
        }
    }

    fn filtered_ids(candidates: &[SessionCandidate], args: &str) -> Vec<String> {
        search_session_rows(candidates, args)
            .into_iter()
            .map(|row| row.name)
            .collect()
    }

    #[test]
    fn test_search_rows_keeps_the_candidate_order() {
        // 同一档里：候选序（树序，由 fetch 时定下）原样透传。
        let candidates = vec![
            session("sess-active", "current work", "/a", "idle"),
            session("sess-inactive-old", "older work", "/b", "inactive"),
            session("sess-inactive-newer", "newer work", "/c", "inactive"),
        ];
        assert_eq!(
            filtered_ids(&candidates, "sess"),
            ["sess-active", "sess-inactive-old", "sess-inactive-newer"]
        );
    }

    #[test]
    fn test_search_rows_tiers_keep_order_within_a_tier() {
        // 分层 exact > prefix > contains，层内保持原序（不重排）。
        let candidates = vec![
            session("session-1", "alpha", "/ws-a", "idle"),
            session("x-sess-2", "beta", "/ws-b", "inactive"),
            session("sess", "gamma", "/ws-c", "idle"),
            session("sess-3", "delta", "/ws-d", "inactive"),
        ];
        assert_eq!(
            filtered_ids(&candidates, "sess"),
            ["sess", "session-1", "sess-3", "x-sess-2"]
        );
    }

    #[test]
    fn test_search_rows_matches_workspace_in_the_contains_tier() {
        // workspace 仍可搜索（只是不再渲染 / 不再排序）：`/ss <workspace 片段>`。
        let candidates = vec![
            session("sess-1", "one", "/home/me/wing-agent", "idle"),
            session("sess-2", "two", "/home/me/other", "idle"),
        ];
        assert_eq!(filtered_ids(&candidates, "wing-agent"), ["sess-1"]);

        // 层内顺序 = 候选顺序：两条都命中 workspace 时按原序给出。
        let both = vec![
            session("sess-a", "one", "/home/me/repo", "idle"),
            session("sess-b", "two", "/home/me/repo", "inactive"),
        ];
        assert_eq!(filtered_ids(&both, "repo"), ["sess-a", "sess-b"]);
    }

    #[test]
    fn test_search_rows_rich_rows_carry_status_and_time() {
        let candidates = vec![session("sess-1", "one", "/ws", "working")];
        let rows = search_session_rows(&candidates, "sess");
        let rich = rows[0].rich.as_ref().expect("session rows are rich");
        assert_eq!(rich.status, crate::protocol::SessionStatus::Working);
        assert_eq!(rich.last_active, "07-22 21:41");
        assert_eq!(rich.title, "one");
    }

    // ── 层视图投影（一次一层） ──────────────────────────────────

    /// 把 `child` 挂到 `parent` 名下。
    fn under(mut c: SessionCandidate, parent: &str) -> SessionCandidate {
        c.parent = Some(parent.into());
        c
    }

    /// 最后交互时间（定序与 LAST 列都用它）。
    fn at(mut c: SessionCandidate, stamp: &str) -> SessionCandidate {
        c.last_interaction = stamp.into();
        c
    }

    fn root_query() -> SessionQuery<'static> {
        SessionQuery::default()
    }

    fn ids(panel: &SessionPanel) -> Vec<&str> {
        panel.rows.iter().map(|r| r.name.as_str()).collect()
    }

    fn rich(panel: &SessionPanel, index: usize) -> &RichSessionRow {
        panel.rows[index]
            .rich
            .as_ref()
            .expect("session rows are rich")
    }

    /// 每一行的（后代总数，后代在等你，后代在干活）。
    fn badges(panel: &SessionPanel) -> Vec<(usize, usize, usize)> {
        let crew = |index: usize| {
            let level = rich(panel, index).level.as_ref().expect("level decor");
            (
                level.crew.descendants,
                level.crew.waiting,
                level.crew.working,
            )
        };
        (0..panel.rows.len()).map(crew).collect()
    }

    /// 带「你在这里」标记的行（id）。
    fn marked(panel: &SessionPanel) -> Vec<&str> {
        panel
            .rows
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                rich(panel, *index)
                    .level
                    .as_ref()
                    .expect("level decor")
                    .current
            })
            .map(|(_, row)| row.name.as_str())
            .collect()
    }

    /// 每一行的 LAST 列（层视图 = 子树最大活跃）。
    fn lasts(panel: &SessionPanel) -> Vec<&str> {
        (0..panel.rows.len())
            .map(|index| rich(panel, index).last_active.as_str())
            .collect()
    }

    /// 一个 pinned 任务 + 40 个子会话（现场形状：pin 的父任务把 1 行乘成 1+N 行）。
    fn pinned_crew() -> Vec<SessionCandidate> {
        let mut candidates = vec![at(
            session("boss", "the team", "/ws", "idle"),
            "2026-10-01T00:00:00",
        )];
        for i in 0..40 {
            let status = if i == 0 { "waiting" } else { "idle" };
            candidates.push(under(
                at(
                    session(&format!("kid-{i:02}"), "worker", "/ws", status),
                    "2026-09-01T00:00:00",
                ),
                "boss",
            ));
        }
        candidates
    }

    #[test]
    fn a_pinned_task_with_forty_children_is_one_row_at_the_root_level() {
        let candidates = pinned_crew();
        let panel = session_panel(&candidates, "", root_query());
        assert_eq!(ids(&panel), ["boss"], "根层一行一个任务");
        assert_eq!(
            badges(&panel),
            [(40, 1, 0)],
            "▸40（全部后代）/ ?1（在等你的）/ ●0（不在跑，所以不出现）"
        );
        assert!(panel.chrome == SessionChrome::default(), "根层没有 chrome");
    }

    #[test]
    fn a_level_lists_its_direct_children_only() {
        let candidates = vec![
            session("root", "r", "/ws", "idle"),
            under(session("kid", "k", "/ws", "idle"), "root"),
            under(session("grand", "g", "/ws", "idle"), "kid"),
        ];
        let level = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: Some("root"),
                ..SessionQuery::default()
            },
        );
        assert_eq!(ids(&level), ["kid"], "孙不在这一层");
        assert_eq!(badges(&level), [(1, 0, 0)], "kid 自己的 crew 只有 grand");

        // 面包屑按标题命名当前层；根层没有这一行。
        assert_eq!(level.chrome.breadcrumb.as_deref(), Some("r"));
        assert_eq!(root_query().scope, None);
    }

    #[test]
    fn the_breadcrumb_falls_back_to_the_id_when_the_title_is_missing() {
        let candidates = vec![
            session("scope-id", "", "/ws", "idle"),
            under(session("kid", "k", "/ws", "idle"), "scope-id"),
        ];
        let level = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: Some("scope-id"),
                ..SessionQuery::default()
            },
        );
        assert_eq!(level.chrome.breadcrumb.as_deref(), Some("scope-id"));
    }

    #[test]
    fn the_last_column_is_the_subtree_max_at_every_level() {
        // 父自己很旧，子更新：根层父那一行显示子的时间，进层后**同一个值**还在
        // （进 / 出层不改变任何单元格的值）。
        let candidates = vec![
            at(session("parent", "p", "/ws", "idle"), "2026-01-01T00:00:00"),
            under(
                at(session("kid", "k", "/ws", "idle"), "2026-03-04T05:06:07"),
                "parent",
            ),
        ];
        let root = session_panel(&candidates, "", root_query());
        assert_eq!(lasts(&root), ["03-04 05:06"]);

        let level = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: Some("parent"),
                include_dormant: false,
                current: None,
            },
        );
        assert_eq!(lasts(&level), ["03-04 05:06"]);
    }

    #[test]
    fn the_last_column_is_the_source_row_text_not_a_reformatted_instant() {
        // 父行的时间来自子树活跃**那一行**的原文，与搜索态的行同一个格式化函数。
        let candidates = vec![
            at(session("parent", "p", "/ws", "idle"), "not-a-timestamp"),
            under(
                at(
                    session("kid", "k", "/ws", "idle"),
                    "2026-03-04T05:06:07+08:00",
                ),
                "parent",
            ),
        ];
        let root = session_panel(&candidates, "", root_query());
        assert_eq!(
            lasts(&root),
            [format_last_active("2026-03-04T05:06:07+08:00")],
            "父行取的是子那一行的原文（带偏移也按原文切，不换算成别的钟点）"
        );

        // 整棵子树都解析不了 → 内核回落到父行自己：显示它自己的原文，而不是空白。
        // （这里用一个结构合法、chrono 解析不了的值 —— 渲染得出来，才看得出取的是谁。）
        let garbage = vec![
            at(session("parent", "p", "/ws", "idle"), "9999-99-99T99:99:99"),
            under(at(session("kid", "k", "/ws", "idle"), ""), "parent"),
        ];
        let root = session_panel(&garbage, "", root_query());
        assert_eq!(lasts(&root), ["99-99 99:99"]);
    }

    #[test]
    fn two_spellings_of_one_instant_stay_in_their_own_rows() {
        // 同一微秒的两种书写（手改 metadata）：来源行由内核指出（并列取先算到的那一行），
        // 渲染读那一行的原文。按微秒反查原文的表会让两行互相串行——父行会显示成另一份
        // 书写，这里逐层钉住。
        let candidates = vec![
            session("parent", "p", "/ws", "idle"),
            under(
                at(
                    session("kid-a", "a", "/ws", "idle"),
                    "2026-10-31T16:21:49+08:00",
                ),
                "parent",
            ),
            under(
                at(
                    session("kid-b", "b", "/ws", "idle"),
                    "2026-10-31T08:21:49.000000",
                ),
                "parent",
            ),
        ];
        let root = session_panel(&candidates, "", root_query());
        assert_eq!(
            lasts(&root),
            [format_last_active("2026-10-31T16:21:49+08:00")],
            "父行取的是来源行（kid-a，先算到的那一行）的原文"
        );

        let level = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: Some("parent"),
                ..SessionQuery::default()
            },
        );
        assert_eq!(
            lasts(&level),
            [
                format_last_active("2026-10-31T16:21:49+08:00"),
                format_last_active("2026-10-31T08:21:49.000000"),
            ],
            "每一行的 LAST 只能是它自己那份原文"
        );
    }

    #[test]
    fn dormant_children_are_hidden_counted_and_revealed_by_the_switch() {
        let candidates = vec![
            session("root", "r", "/ws", "idle"),
            under(session("live", "alive", "/ws", "idle"), "root"),
            under(session("parked", "evicted", "/ws", "inactive"), "root"),
        ];
        let hidden = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: Some("root"),
                include_dormant: false,
                current: None,
            },
        );
        assert_eq!(ids(&hidden), ["live"]);
        assert_eq!(
            hidden.chrome.footnote,
            Some(DormantFootnote {
                hidden: 1,
                shown: false
            }),
            "尾注说出被收起的数量"
        );

        let all = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: Some("root"),
                include_dormant: true,
                current: None,
            },
        );
        assert_eq!(
            ids(&all),
            ["live", "parked"],
            "开关打开：行集变大、顺序不变"
        );
        assert_eq!(
            all.chrome.footnote,
            Some(DormantFootnote {
                hidden: 1,
                shown: true
            }),
            "打开时尾注说的是当前状态"
        );
    }

    #[test]
    fn a_fully_dormant_crew_is_an_empty_level_with_chrome() {
        let candidates = vec![
            session("root", "r", "/ws", "idle"),
            under(session("parked", "evicted", "/ws", "inactive"), "root"),
        ];
        let level = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: Some("root"),
                include_dormant: false,
                current: None,
            },
        );
        assert!(level.rows.is_empty());
        assert_eq!(level.chrome.breadcrumb.as_deref(), Some("r"));
        assert_eq!(
            level.chrome.footnote,
            Some(DormantFootnote {
                hidden: 1,
                shown: false
            })
        );
        assert_eq!(level.chrome.height(), 2);
    }

    #[test]
    fn the_current_session_marks_its_ancestor_chain_and_never_more_than_one_per_level() {
        // root → mid → me。每一层点亮的都是「通往我的那一行」。
        let candidates = vec![
            session("root", "r", "/ws", "idle"),
            under(session("mid", "m", "/ws", "idle"), "root"),
            under(session("me", "mine", "/ws", "idle"), "mid"),
            session("sibling", "s", "/ws", "idle"),
        ];
        let query = |scope| SessionQuery {
            scope,
            include_dormant: false,
            current: Some("me"),
        };

        let root = session_panel(&candidates, "", query(None));
        assert_eq!(ids(&root), ["root", "sibling"]);
        assert_eq!(marked(&root), ["root"], "根层标的是我的根祖先");

        let mid = session_panel(&candidates, "", query(Some("root")));
        assert_eq!(marked(&mid), ["mid"]);

        let leaf = session_panel(&candidates, "", query(Some("mid")));
        assert_eq!(marked(&leaf), ["me"], "我的那一层标的正是我自己");
    }

    #[test]
    fn a_current_session_outside_the_payload_marks_nothing() {
        let candidates = vec![session("root", "r", "/ws", "idle")];
        let panel = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: None,
                include_dormant: false,
                current: Some("ghost"),
            },
        );
        assert_eq!(ids(&panel), ["root"]);
        assert!(marked(&panel).is_empty());
    }

    #[test]
    fn a_cycle_leaves_no_crew_to_enter() {
        // A ↔ B（只可能来自手改 metadata）：内核把两者都升为根、断开那条边。所以两个
        // 根都没有 crew——面板因此不给「能进去」的假象（按原始 parent 边数就会给）。
        let cyclic = vec![
            under(session("A", "a", "/ws", "idle"), "B"),
            under(session("B", "b", "/ws", "idle"), "A"),
        ];
        let level = session_panel(&cyclic, "", root_query());
        assert_eq!(ids(&level), ["A", "B"]);
        assert_eq!(badges(&level), [(0, 0, 0), (0, 0, 0)], "两个根都没有后代");
    }

    #[test]
    fn an_unknown_scope_is_an_empty_level_and_names_the_id() {
        let candidates = vec![session("root", "r", "/ws", "idle")];
        let panel = session_panel(
            &candidates,
            "",
            SessionQuery {
                scope: Some("gone"),
                include_dormant: false,
                current: None,
            },
        );
        assert!(panel.rows.is_empty());
        assert_eq!(panel.chrome.breadcrumb.as_deref(), Some("gone"));
        assert_eq!(panel.chrome.footnote, None);
    }

    #[test]
    fn the_search_state_carries_no_level_decor_and_no_chrome() {
        let candidates = vec![
            session("root", "rustacean", "/ws", "idle"),
            under(session("kid", "k", "/ws", "idle"), "root"),
        ];
        let panel = session_panel(
            &candidates,
            "rust",
            SessionQuery {
                scope: Some("root"),
                include_dormant: true,
                current: Some("kid"),
            },
        );
        assert_eq!(ids(&panel), ["root"]);
        assert_eq!(panel.chrome, SessionChrome::default());
        assert!(
            panel
                .rows
                .iter()
                .all(|r| r.rich.as_ref().unwrap().level.is_none()),
            "搜索态不带徽章 / 标记"
        );
    }
}
