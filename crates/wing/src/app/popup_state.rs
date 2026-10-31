//! PopupState — aggregated popup state and candidate cache.

use crate::ui::popup::ActivePopup;
use crate::ui::popup::command::CandidateCache;
use crate::ui::popup::command::PopupAction;
use crate::ui::popup::command::SessionQuery;

/// Aggregated popup state: active popup and candidate cache.
#[derive(Default)]
pub struct PopupState {
    /// Current popup state (None when inactive).
    pub active: ActivePopup,
    /// Cached sub-command candidates.
    pub cache: CandidateCache,
    /// 会话面板的层视图状态（当前层路径 + dormant 开关）。
    pub level_view: LevelView,
}

impl PopupState {
    /// Update popup state based on input text.
    ///
    /// Returns the popup action if a candidate fetch is needed.
    pub fn update_from_input(&mut self, text: &str, current: &str) -> Option<PopupAction> {
        let query = self.level_view.query(current);
        let action = self.active.update_from_input(text, &self.cache, query);
        // 面板不在会话子命令态（关掉了 / 退回了等待首帧 / 换了个命令）→ 层状态归零：
        // `scope` 不跨面板记忆。搜索态仍算会话子命令态，所以清掉过滤词能回到原来那一层。
        if self.active.session_panel_command().is_none() {
            self.level_view.reset();
        }
        action
    }

    /// Reset all popup state (e.g., on session switch).
    pub fn reset(&mut self) {
        self.cache.clear();
        self.close_active();
    }

    /// 关掉当前 popup —— 唯一的关闭口：层的状态随面板一起结束。
    pub fn close_active(&mut self) {
        self.active = ActivePopup::None;
        self.level_view.reset();
    }

    /// Get the popup height (0 if no popup).
    pub fn height(&self) -> u16 {
        self.active.height()
    }
}

/// 会话面板的层视图状态：从根层逐层进入的**路径** + 当前层是否含 dormant。
///
/// 住在 App（[`PopupState`]）而不是 `ActivePopup`：popup 每次按键都整体重建，状态得
/// 活得更久（一个面板生命周期内保持，关闭即归零）。
///
/// 用路径而不是单个 `scope`：`←` 要回到父层、退回时光标要落回「你刚才进来的那一行」，
/// 而「父是谁」只有森林知道——路径的 `last()` 就是当前层，`pop()` 一步退一层，两者都
/// 不需要再问拓扑。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LevelView {
    path: Vec<String>,
    include_dormant: bool,
}

impl LevelView {
    /// 当前层（`None` = 根层 = 任务层）。
    pub fn scope(&self) -> Option<&str> {
        self.path.last().map(String::as_str)
    }

    /// 层数（0 = 根层）。
    pub fn depth(&self) -> usize {
        self.path.len()
    }

    /// 进入 `id` 的 crew（下一层）。
    pub fn enter(&mut self, id: &str) {
        self.path.push(id.to_string());
    }

    /// 退回上一层，返回刚离开的那一层（根层返回 `None`，路径不动）。
    pub fn back(&mut self) -> Option<String> {
        self.path.pop()
    }

    /// 翻转 dormant 开关。
    pub fn toggle_dormant(&mut self) {
        self.include_dormant = !self.include_dormant;
    }

    /// 回到根层，dormant 开关保留（它只随面板生命周期复位）。
    fn back_to_root(&mut self) {
        self.path.clear();
    }

    /// 面板关闭：回根层 + dormant 关。
    pub fn reset(&mut self) {
        self.back_to_root();
        self.include_dormant = false;
    }

    /// payload 变了：路径上任一层不在新列表里（归档 / 删除）→ 整条路径清空回根层。
    ///
    /// 为什么是整条而不是截断到还能走通的那一段：更深层的入口都得从根一层层进去，
    /// 中间断掉就等于没有入口。
    pub fn retain_existing(&mut self, mut exists: impl FnMut(&str) -> bool) {
        if !self.path.iter().all(|id| exists(id)) {
            self.back_to_root();
        }
    }

    /// 投影参数（当前会话由 App 给：它的祖先链在层视图里点亮）。
    pub fn query<'a>(&'a self, current: &'a str) -> SessionQuery<'a> {
        SessionQuery {
            scope: self.scope(),
            include_dormant: self.include_dormant,
            current: Some(current),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entering_and_going_back_walks_one_level_at_a_time() {
        let mut view = LevelView::default();
        assert_eq!(view.scope(), None);
        assert_eq!(view.depth(), 0);

        view.enter("A");
        view.enter("B");
        assert_eq!((view.scope(), view.depth()), (Some("B"), 2));
        // 退回：返回的是刚离开的那一层（调用方据此把光标放回那一行）。
        assert_eq!(view.back().as_deref(), Some("B"));
        assert_eq!(view.scope(), Some("A"));
        assert_eq!(view.back().as_deref(), Some("A"));
        assert_eq!(view.scope(), None);
        // 根层再退 = no-op。
        assert_eq!(view.back(), None);
        assert_eq!(view.depth(), 0);
    }

    #[test]
    fn the_dormant_switch_outlives_level_changes_but_not_the_panel() {
        let mut view = LevelView::default();
        view.toggle_dormant();
        assert!(view.query("me").include_dormant);
        view.enter("A");
        assert!(view.query("me").include_dormant, "切层不清开关");
        view.back();
        assert!(view.query("me").include_dormant);
        view.reset();
        assert!(!view.query("me").include_dormant, "面板关闭复位");
        assert_eq!(view.depth(), 0);
    }

    #[test]
    fn a_vanished_ancestor_sends_the_whole_path_back_to_the_root() {
        let mut view = LevelView::default();
        view.enter("A");
        view.enter("B");

        // 路径完整：不动。
        view.retain_existing(|id| ["A", "B"].contains(&id));
        assert_eq!(view.depth(), 2);

        // 中间那一层没了：整条路径作废（更深层的入口都得从根走）。
        view.retain_existing(|id| id == "B");
        assert_eq!(view.depth(), 0);
        assert_eq!(view.scope(), None);
    }

    #[test]
    fn the_query_carries_the_scope_dormant_switch_and_current_session() {
        let mut view = LevelView::default();
        let query = view.query("me");
        assert_eq!(query.scope, None);
        assert!(!query.include_dormant);
        assert_eq!(query.current, Some("me"));

        view.enter("A");
        view.toggle_dormant();
        let query = view.query("me");
        assert_eq!(query.scope, Some("A"));
        assert!(query.include_dormant);
    }
}
