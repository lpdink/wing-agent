//! ModelPanel — the `/model` adapter on the selection-panel kernel.
//!
//! Provider tabs (pages) × model rows:
//! - `←`/`→` switch provider (wraps), `↑`/`↓` move the model cursor (wraps,
//!   per-provider memory),
//! - `Enter` applies the highlighted `(provider, model)` in one keypress —
//!   there is no confirm page (model switching is a cheap, reversible act),
//! - `Esc` cancels (the app closes the panel; no request is sent),
//! - provider tabs and model rows are windowed by the kernel (≤ 5 visible)
//!   with `‹`/`›` markers rendered by the shared panel renderer.
//!
//! Opening preselects the session's current `(provider, model)`: the cursor
//! lands on that provider page / model row and a `●` mark (the kernel's
//! committed row) identifies the model currently in use. An unknown provider
//! or model falls back to the first page / first row without a mark.
//!
//! The applied pair is always explicit: `Enter` produces the provider name
//! and model name together, and the app dispatches them as-is — no
//! name-based re-resolution, so same-named models across providers cannot be
//! confused.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;

use crate::app::selection_panel::PANEL_WINDOW;
use crate::app::selection_panel::PageKind;
use crate::app::selection_panel::SelectionPanel;
use wing_api_client::models::ProviderModels;

/// Outcome of a key event for the app to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelPanelAction {
    /// Key consumed, nothing to do.
    None,
    /// Enter on a model row: apply this (provider, model) pair.
    Apply { provider: String, model: String },
    /// Esc: close the panel without changing anything.
    Cancel,
}

/// Interactive state of the model picker.
#[derive(Debug, Clone)]
pub struct ModelPanel {
    /// Provider groups + their models (from the last successful fetch).
    sources: Vec<ProviderModels>,
    /// Active provider page.
    current: usize,
    /// Per-provider model cursor.
    cursors: Vec<usize>,
    /// Per-provider committed row: the session's active model (`●` mark).
    committed: Vec<Option<usize>>,
}

impl ModelPanel {
    /// Build a panel from fetched sources, preselecting `current` — the
    /// session's active `(provider, model)` pair when known.
    pub fn new(sources: Vec<ProviderModels>, current: Option<(&str, &str)>) -> Self {
        let mut panel = Self {
            cursors: vec![0; sources.len()],
            committed: vec![None; sources.len()],
            current: 0,
            sources,
        };
        panel.preselect(current);
        panel
    }

    /// In-place refresh: replace the sources, keeping the active page, cursor
    /// and mark where they are still valid (kernel fallback otherwise). An
    /// empty update is ignored so the last good data stays on screen.
    pub fn set_sources(&mut self, sources: Vec<ProviderModels>) {
        if sources.is_empty() {
            return;
        }
        self.sources = sources;
        self.cursors.resize(self.sources.len(), 0);
        self.committed.resize(self.sources.len(), None);
        self.clamp_after_refresh();
    }

    /// Preselect the session's current `(provider, model)`: provider page,
    /// model row and `●` mark. Unknown provider/model falls back to the first
    /// page / row without a mark.
    fn preselect(&mut self, pair: Option<(&str, &str)>) {
        let Some((provider, model)) = pair else {
            return;
        };
        let Some(page) = self.sources.iter().position(|p| p.provider == provider) else {
            return;
        };
        self.current = page;
        let Some(row) = self.sources[page].models.iter().position(|m| m == model) else {
            return;
        };
        self.cursors[page] = row;
        self.committed[page] = Some(row);
    }

    /// Handle one key event. `Esc` cancels the panel — the app owns closing.
    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPanelAction {
        match key.code {
            KeyCode::Left => {
                self.move_page(-1);
                ModelPanelAction::None
            }
            KeyCode::Right => {
                self.move_page(1);
                ModelPanelAction::None
            }
            KeyCode::Up => {
                self.move_cursor(-1);
                ModelPanelAction::None
            }
            KeyCode::Down => {
                self.move_cursor(1);
                ModelPanelAction::None
            }
            KeyCode::Enter => self.apply_current(),
            KeyCode::Esc => ModelPanelAction::Cancel,
            _ => ModelPanelAction::None,
        }
    }

    /// Enter: apply the highlighted pair. No-op on an empty model list — there
    /// is nothing to apply, and the user can still switch providers.
    fn apply_current(&mut self) -> ModelPanelAction {
        let page = self.current_page();
        let Some(group) = self.sources.get(page) else {
            return ModelPanelAction::None;
        };
        let Some(model) = group.models.get(self.cursor_at(page)) else {
            return ModelPanelAction::None;
        };
        ModelPanelAction::Apply {
            provider: group.provider.clone(),
            model: model.clone(),
        }
    }

    // ── Rendering accessors ─────────────────────────────────────

    /// Provider groups (tab bar + rows source).
    pub fn sources(&self) -> &[ProviderModels] {
        &self.sources
    }

    /// Models of the active provider.
    pub fn models(&self) -> &[String] {
        self.sources
            .get(self.current_page())
            .map_or(&[], |group| group.models.as_slice())
    }

    /// Cursor row on the active provider.
    pub fn cursor(&self) -> usize {
        self.cursor_at(self.current_page())
    }

    /// Rendered height: tab bar (1) + visible rows (min 1 empty-state row, max
    /// [`PANEL_WINDOW`]) + hint footer (1).
    pub fn height(&self) -> u16 {
        (self.models().len().clamp(1, PANEL_WINDOW) + 2) as u16
    }
}

/// ModelPanel as a selection-panel adapter: each provider is an options page
/// (its models), all pages kernel-navigated — no custom pages.
impl SelectionPanel for ModelPanel {
    fn page_count(&self) -> usize {
        self.sources.len()
    }

    fn page_kind(&self, page: usize) -> PageKind {
        PageKind::Options {
            rows: self.sources.get(page).map_or(0, |group| group.models.len()),
        }
    }

    fn current_page(&self) -> usize {
        self.current
    }

    fn set_current_page(&mut self, page: usize) {
        self.current = page;
    }

    fn cursor_at(&self, page: usize) -> usize {
        self.cursors.get(page).copied().unwrap_or(0)
    }

    fn set_cursor_at(&mut self, page: usize, row: usize) {
        if let Some(cursor) = self.cursors.get_mut(page) {
            *cursor = row;
        }
    }

    fn committed_at(&self, page: usize) -> Option<usize> {
        self.committed.get(page).copied().flatten()
    }

    fn set_committed_at(&mut self, page: usize, row: Option<usize>) {
        if let Some(slot) = self.committed.get_mut(page) {
            *slot = row;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn group(provider: &str, models: &[&str]) -> ProviderModels {
        ProviderModels {
            provider: provider.into(),
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    /// Two providers exposing the same model name (`shared`).
    fn same_name_sources() -> Vec<ProviderModels> {
        vec![
            group("dashscope", &["shared", "only-a"]),
            group("dashscope-openai", &["shared", "only-b"]),
        ]
    }

    // ── Same-name regression ────────────────────────────────────

    #[test]
    fn same_name_model_resolves_to_the_selected_provider() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        // First provider, first row: `shared` → dashscope.
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                provider: "dashscope".into(),
                model: "shared".into(),
            }
        );
        // Switch to the second provider and select its `shared` → the SECOND
        // provider must be applied (the old first-match resolution bug).
        let mut panel = ModelPanel::new(same_name_sources(), None);
        panel.handle_key(key(KeyCode::Right));
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                provider: "dashscope-openai".into(),
                model: "shared".into(),
            }
        );
    }

    #[test]
    fn page_switch_wraps_and_keeps_per_provider_cursor() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        panel.handle_key(key(KeyCode::Down)); // provider 0 → row 1 (only-a)
        panel.handle_key(key(KeyCode::Right)); // provider 1
        panel.handle_key(key(KeyCode::Down)); // → row 1 (only-b)
        panel.handle_key(key(KeyCode::Left)); // wrap back to provider 0
        assert_eq!(panel.cursor(), 1, "provider 0 keeps its cursor");
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                provider: "dashscope".into(),
                model: "only-a".into(),
            }
        );
        panel.handle_key(key(KeyCode::Left)); // wrap: 0 → last provider
        assert_eq!(panel.cursor(), 1);
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                provider: "dashscope-openai".into(),
                model: "only-b".into(),
            }
        );
    }

    // ── Preselect ───────────────────────────────────────────────

    #[test]
    fn preselects_current_pair_with_marker() {
        let panel = ModelPanel::new(same_name_sources(), Some(("dashscope-openai", "only-b")));
        assert_eq!(panel.current_page(), 1, "opens on the current provider");
        assert_eq!(panel.cursor(), 1, "cursor on the current model");
        assert_eq!(
            panel.committed_at(1),
            Some(1),
            "the current model carries the ● mark"
        );
        assert_eq!(panel.committed_at(0), None, "other pages are unmarked");
    }

    #[test]
    fn unknown_provider_falls_back_to_the_first_page() {
        let panel = ModelPanel::new(same_name_sources(), Some(("nope", "only-b")));
        assert_eq!(panel.current_page(), 0);
        assert_eq!(panel.cursor(), 0);
        assert_eq!(panel.committed_at(0), None);
    }

    #[test]
    fn known_provider_unknown_model_selects_page_without_mark() {
        let panel = ModelPanel::new(same_name_sources(), Some(("dashscope-openai", "nope")));
        assert_eq!(panel.current_page(), 1);
        assert_eq!(panel.cursor(), 0);
        assert_eq!(panel.committed_at(1), None);
    }

    #[test]
    fn missing_pair_falls_back_to_the_first_page() {
        let panel = ModelPanel::new(same_name_sources(), None);
        assert_eq!(panel.current_page(), 0);
        assert_eq!(panel.cursor(), 0);
        assert_eq!(panel.committed_at(0), None);
    }

    // ── Empty page ──────────────────────────────────────────────

    #[test]
    fn empty_model_list_page_does_not_apply_and_can_be_left() {
        let sources = vec![group("full", &["m1"]), group("empty", &[])];
        let mut panel = ModelPanel::new(sources, None);
        panel.handle_key(key(KeyCode::Right)); // → empty page
        assert_eq!(panel.models().len(), 0);
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::None,
            "Enter on an empty page must not produce a request"
        );
        panel.handle_key(key(KeyCode::Up)); // cursor stays put
        assert_eq!(panel.cursor(), 0);
        panel.handle_key(key(KeyCode::Right)); // can move on
        assert_eq!(panel.current_page(), 0, "wraps back to the full page");
    }

    // ── Esc / other keys ────────────────────────────────────────

    #[test]
    fn esc_cancels_without_applying() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        assert_eq!(
            panel.handle_key(key(KeyCode::Esc)),
            ModelPanelAction::Cancel
        );
    }

    #[test]
    fn unrelated_keys_are_ignored() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        for code in [KeyCode::Char('x'), KeyCode::Tab, KeyCode::Home] {
            assert_eq!(panel.handle_key(key(code)), ModelPanelAction::None);
        }
        assert_eq!(panel.current_page(), 0);
    }

    // ── Refresh ─────────────────────────────────────────────────

    #[test]
    fn refresh_keeps_page_cursor_and_marker() {
        let mut panel = ModelPanel::new(same_name_sources(), Some(("dashscope-openai", "only-b")));
        let refreshed = vec![
            group("dashscope", &["shared", "only-a", "new-a"]),
            group("dashscope-openai", &["shared", "only-b", "new-b"]),
        ];
        panel.set_sources(refreshed);
        assert_eq!(panel.current_page(), 1);
        assert_eq!(panel.cursor(), 1);
        assert_eq!(panel.committed_at(1), Some(1));
    }

    #[test]
    fn refresh_falls_back_when_the_current_page_vanishes() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        panel.handle_key(key(KeyCode::Right)); // page 1
        panel.set_sources(vec![group("dashscope", &["shared"])]);
        assert_eq!(panel.current_page(), 0, "missing provider → first page");
        assert_eq!(panel.cursor(), 0);
    }

    #[test]
    fn refresh_clamps_cursor_and_clears_vanished_mark() {
        let mut panel = ModelPanel::new(vec![group("p", &["a", "b", "c"])], Some(("p", "c")));
        panel.set_sources(vec![group("p", &["a"])]);
        assert_eq!(panel.cursor(), 0, "cursor clamps to the remaining row");
        assert_eq!(panel.committed_at(0), None, "vanished model loses its mark");
    }

    #[test]
    fn empty_refresh_keeps_the_last_good_data() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        panel.set_sources(vec![]);
        assert_eq!(panel.page_count(), 2);
        assert_eq!(panel.models().len(), 2);
    }

    // ── Height / window ─────────────────────────────────────────

    #[test]
    fn height_reflects_visible_window() {
        let panel = ModelPanel::new(vec![group("p", &["m1", "m2", "m3"])], None);
        assert_eq!(panel.height(), 5, "tabs + 3 rows + hint");
        let many: Vec<&str> = (0..9).map(|_| "m").collect();
        let panel = ModelPanel::new(vec![group("p", &many)], None);
        assert_eq!(panel.height(), 7, "tabs + 5 visible rows + hint");
        let panel = ModelPanel::new(vec![group("p", &[])], None);
        assert_eq!(panel.height(), 3, "tabs + empty-state row + hint");
    }

    #[test]
    fn window_on_long_model_list_follows_the_cursor() {
        use crate::app::selection_panel::window_range;
        let many: Vec<String> = (0..8).map(|i| format!("m{i}")).collect();
        let mut panel = ModelPanel::new(
            vec![ProviderModels {
                provider: "p".into(),
                models: many,
            }],
            None,
        );
        for _ in 0..5 {
            panel.handle_key(key(KeyCode::Down));
        }
        let range = window_range(panel.cursor(), panel.models().len(), PANEL_WINDOW);
        assert_eq!(range, 1..6, "cursor on row 5 slides the window");
        assert!(range.contains(&panel.cursor()));
    }
}
