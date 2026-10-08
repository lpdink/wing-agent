//! ModelPanel — the `/model` adapter on the selection-panel kernel.
//!
//! Provider tabs (pages) × model rows:
//! - `←`/`→` switch provider (clamped at the ends — no wrap-around),
//! - `↑`/`↓` move the model cursor (clamped at the ends; per-provider memory),
//! - `Enter` applies the highlighted model's **id** in one keypress —
//!   there is no confirm page (model switching is a cheap, reversible act),
//! - `Esc` cancels (the app closes the panel; no request is sent),
//! - provider tabs and model rows are windowed by the kernel (≤ 5 visible),
//!   the cursor / active tab stays centered while scrolling, the window is
//!   pinned at the ends, and there are **no** indicator glyphs (`‹`/`›`)
//!   — the rows stay column-aligned instead.
//!
//! Opening preselects the session's current model **by id**: the cursor lands
//! on that row (its provider page becomes active) and a `●` mark (the kernel's
//! committed row) identifies the model currently in use. An unknown id (or a
//! session that has none — old metadata without a reference word) falls back to
//! the first page / first row without a mark.
//!
//! Identity is the **model id** (`providers[].models[].id`, globally unique):
//! `Enter` produces exactly the id the gateway's `session/update` takes —
//! no name-based re-resolution, so same-named models across providers cannot
//! be confused (they carry different ids), and the provider is only the tab
//! grouping / display dimension.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;

use wing_api_client::models::ModelDetail;
use wing_api_client::models::ProviderModels;

use super::PageKind;
use super::SelectionPanel;

/// Outcome of a key event for the app to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelPanelAction {
    /// Key consumed, nothing to do.
    None,
    /// Enter on a model row: apply this model id.
    Apply { model_id: String },
    /// Esc: close the panel without changing anything.
    Cancel,
}

/// Interactive state of the model picker.
#[derive(Debug, Clone)]
pub struct ModelPanel {
    /// Provider groups + their declared models (from the last successful fetch).
    sources: Vec<ProviderModels>,
    /// Active provider page.
    current: usize,
    /// Per-provider model cursor.
    cursors: Vec<usize>,
    /// Per-provider committed row: the session's active model (`●` mark).
    committed: Vec<Option<usize>>,
}

impl ModelPanel {
    /// Build a panel from fetched sources, preselecting the session's active
    /// model id when known.
    pub fn new(sources: Vec<ProviderModels>, current_model_id: Option<&str>) -> Self {
        let mut panel = Self {
            cursors: vec![0; sources.len()],
            committed: vec![None; sources.len()],
            current: 0,
            sources,
        };
        panel.preselect(current_model_id);
        panel
    }

    /// In-place refresh: replace the sources, keeping the active page, cursor
    /// and mark where they are still valid.  Cursor and committed mark are
    /// re-resolved by **model id** (not index), so re-ordering or insertion in
    /// the catalog never shifts the selection to a different model — and since
    /// ids are globally unique, a provider reshuffle cannot collide either.
    /// An empty update is ignored so the last good data stays on screen.
    pub fn set_sources(&mut self, sources: Vec<ProviderModels>) {
        if sources.is_empty() {
            return;
        }
        // Snapshot cursor/committed by model id (identity), plus the active page.
        let prev: Vec<(String, Option<String>, Option<String>)> = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let row = self.cursor_at(i);
                let cursor = g.models.get(row).map(|m| m.id.clone());
                let committed = self
                    .committed_at(i)
                    .and_then(|r| g.models.get(r).map(|m| m.id.clone()));
                (g.provider.clone(), cursor, committed)
            })
            .collect();
        let prev_active_name: Option<String> = self
            .sources
            .get(self.current_page())
            .map(|g| g.provider.clone());

        self.sources = sources;
        self.cursors = vec![0; self.sources.len()];
        self.committed = vec![None; self.sources.len()];
        self.current = 0;

        for (i, group) in self.sources.iter().enumerate() {
            if let Some((_, cursor, committed)) = prev.iter().find(|(p, _, _)| *p == group.provider)
            {
                if let Some(id) = cursor {
                    self.cursors[i] = group.models.iter().position(|m| &m.id == id).unwrap_or(0);
                }
                if let Some(id) = committed {
                    self.committed[i] = group.models.iter().position(|m| &m.id == id);
                }
            }
            // Restore the active page by provider name.
            if prev_active_name.as_deref() == Some(group.provider.as_str()) {
                self.current = i;
            }
        }
    }

    /// Preselect the session's current model id: provider page, model row and
    /// `●` mark. Unknown / absent id falls back to the first page / row
    /// without a mark (the frontend never invents an id).
    fn preselect(&mut self, model_id: Option<&str>) {
        let Some(model_id) = model_id.filter(|id| !id.trim().is_empty()) else {
            return;
        };
        let Some((page, row)) = self.sources.iter().enumerate().find_map(|(page, group)| {
            group
                .models
                .iter()
                .position(|detail| detail.id == model_id)
                .map(|row| (page, row))
        }) else {
            return;
        };
        self.current = page;
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

    /// Enter: apply the highlighted model id. No-op on an empty model list —
    /// there is nothing to apply, and the user can still switch providers.
    fn apply_current(&mut self) -> ModelPanelAction {
        let page = self.current_page();
        let Some(group) = self.sources.get(page) else {
            return ModelPanelAction::None;
        };
        let Some(detail) = group.models.get(self.cursor_at(page)) else {
            return ModelPanelAction::None;
        };
        ModelPanelAction::Apply {
            model_id: detail.id.clone(),
        }
    }

    // ── Rendering accessors ─────────────────────────────────────

    /// Provider groups (tab bar + rows source).
    pub fn sources(&self) -> &[ProviderModels] {
        &self.sources
    }

    /// Model declarations of the active provider.
    pub fn models(&self) -> &[ModelDetail] {
        self.sources
            .get(self.current_page())
            .map_or(&[], |group| group.models.as_slice())
    }

    /// Display label for a row on the active provider page: the model's
    /// declared `display_name` when present (non-empty), otherwise its call
    /// name. Rendering-only — Apply / cursor / mark keep resolving by the id
    /// (`models()`), so the identity layer never sees this.
    pub fn label_at(&self, row: usize) -> &str {
        let Some(group) = self.sources.get(self.current_page()) else {
            return "";
        };
        let Some(detail) = group.models.get(row) else {
            return "";
        };
        detail.display_label()
    }

    /// Cursor row on the active provider.
    pub fn cursor(&self) -> usize {
        self.cursor_at(self.current_page())
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

    fn detail(id: &str, name: &str) -> ModelDetail {
        ModelDetail {
            id: id.into(),
            name: name.into(),
            display_name: None,
            description: None,
            capabilities: Default::default(),
        }
    }

    /// Provider group where each model's id equals its name.
    fn group(provider: &str, models: &[&str]) -> ProviderModels {
        ProviderModels {
            provider: provider.into(),
            models: models.iter().map(|m| detail(m, m)).collect(),
        }
    }

    /// Provider group declaring a display label for the given model id.
    fn group_with_labels(
        provider: &str,
        models: &[&str],
        labels: &[(&str, &str)],
    ) -> ProviderModels {
        ProviderModels {
            provider: provider.into(),
            models: models
                .iter()
                .map(|id| {
                    let label = labels
                        .iter()
                        .find(|(model, _)| model == id)
                        .map(|(_, label)| (*label).to_string());
                    ModelDetail {
                        id: (*id).to_string(),
                        name: (*id).to_string(),
                        display_name: label,
                        description: None,
                        capabilities: Default::default(),
                    }
                })
                .collect(),
        }
    }

    /// Two providers exposing same-named models under **different ids**.
    fn same_name_sources() -> Vec<ProviderModels> {
        vec![
            ProviderModels {
                provider: "dashscope".into(),
                models: vec![detail("shared-a", "shared"), detail("only-a", "only-a")],
            },
            ProviderModels {
                provider: "dashscope-openai".into(),
                models: vec![detail("shared-b", "shared"), detail("only-b", "only-b")],
            },
        ]
    }

    // ── Id-keyed apply ──────────────────────────────────────────

    #[test]
    fn apply_carries_the_id_of_the_selected_row() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        // Second page, first row: the same *name* as the first page's `shared`,
        // but its own id — the tab the user selected decides.
        panel.handle_key(key(KeyCode::Right));
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                model_id: "shared-b".into(),
            }
        );
    }

    #[test]
    fn page_switch_clamps_and_keeps_per_provider_cursor() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        panel.handle_key(key(KeyCode::Down)); // provider 0 → row 1 (only-a)
        panel.handle_key(key(KeyCode::Right)); // provider 1
        panel.handle_key(key(KeyCode::Down)); // → row 1 (only-b)
        panel.handle_key(key(KeyCode::Left)); // back to provider 0
        assert_eq!(panel.cursor(), 1, "provider 0 keeps its cursor");
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                model_id: "only-a".into(),
            }
        );
        // ← at the first provider stays put; → walks back to the second one
        // with its remembered cursor.
        panel.handle_key(key(KeyCode::Left));
        assert_eq!(panel.current_page(), 0, "clamped at the first provider");
        panel.handle_key(key(KeyCode::Right));
        assert_eq!(panel.cursor(), 1);
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                model_id: "only-b".into(),
            }
        );
    }

    // ── Preselect (by id) ───────────────────────────────────────

    #[test]
    fn preselects_current_id_with_marker() {
        let panel = ModelPanel::new(same_name_sources(), Some("only-b"));
        assert_eq!(panel.current_page(), 1, "opens on the id's provider");
        assert_eq!(panel.cursor(), 1, "cursor on the current model");
        assert_eq!(
            panel.committed_at(1),
            Some(1),
            "the current model carries the ● mark"
        );
        assert_eq!(panel.committed_at(0), None, "other pages are unmarked");
    }

    #[test]
    fn unknown_id_falls_back_to_the_first_page() {
        let panel = ModelPanel::new(same_name_sources(), Some("nope"));
        assert_eq!(panel.current_page(), 0);
        assert_eq!(panel.cursor(), 0);
        assert_eq!(panel.committed_at(0), None);
    }

    #[test]
    fn missing_or_blank_id_falls_back_to_the_first_page() {
        // 旧会话（metadata 无 id）：没有可匹配的引用词 → 无标记（不发明 id）。
        let panel = ModelPanel::new(same_name_sources(), None);
        assert_eq!(panel.current_page(), 0);
        assert_eq!(panel.cursor(), 0);
        assert_eq!(panel.committed_at(0), None);

        let panel = ModelPanel::new(same_name_sources(), Some("   "));
        assert_eq!(panel.current_page(), 0);
        assert_eq!(panel.committed_at(0), None);
    }

    /// 同名模型跨 provider：id 各自独立 → 各自独立预选（旧世界靠 (provider, name)
    /// 消歧，现在 id 本身就是消歧结果）。
    #[test]
    fn same_name_models_are_preselected_by_their_own_ids() {
        let panel = ModelPanel::new(same_name_sources(), Some("shared-b"));
        assert_eq!(panel.current_page(), 1);
        assert_eq!(panel.committed_at(1), Some(0));
        let panel = ModelPanel::new(same_name_sources(), Some("shared-a"));
        assert_eq!(panel.current_page(), 0);
        assert_eq!(panel.committed_at(0), Some(0));
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
        panel.handle_key(key(KeyCode::Left)); // can move on
        assert_eq!(panel.current_page(), 0, "back to the full page");
    }

    // ── Display labels (display_name) ───────────────────────────

    #[test]
    fn label_at_prefers_display_name_and_falls_back_to_call_name() {
        let sources = vec![group_with_labels(
            "qoder",
            &["dfmodel", "bare"],
            &[("dfmodel", "DeepSeek-Flash")],
        )];
        let panel = ModelPanel::new(sources, None);
        assert_eq!(panel.label_at(0), "DeepSeek-Flash");
        assert_eq!(
            panel.label_at(1),
            "bare",
            "uncovered model falls back to its call name"
        );
    }

    #[test]
    fn label_at_tolerates_empty_page_and_out_of_range_row() {
        let panel = ModelPanel::new(vec![group("empty", &[])], None);
        assert_eq!(panel.label_at(0), "");
        assert_eq!(panel.label_at(99), "");
        let panel = ModelPanel::new(vec![], None);
        assert_eq!(panel.label_at(0), "");
    }

    /// The identity layer is untouched: with display names present, the
    /// cursor and the applied value still resolve by the id.
    #[test]
    fn apply_uses_the_id_even_when_display_names_exist() {
        let mut panel = ModelPanel::new(
            vec![group_with_labels(
                "qoder",
                &["dfmodel", "other"],
                &[("dfmodel", "DeepSeek-Flash")],
            )],
            Some("dfmodel"),
        );
        assert_eq!(
            panel.label_at(panel.cursor()),
            "DeepSeek-Flash",
            "row renders the display name"
        );
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                model_id: "dfmodel".into(),
            },
            "Apply carries the id, never the display name"
        );
    }

    /// id ≠ name：行渲染调用名/展示名，Apply 发 id。
    #[test]
    fn apply_carries_the_id_when_it_differs_from_the_call_name() {
        let sources = vec![ProviderModels {
            provider: "qoder".into(),
            models: vec![ModelDetail {
                id: "ds-flash".into(),
                name: "dfmodel-2026".into(),
                display_name: Some("DeepSeek-Flash".into()),
                description: None,
                capabilities: Default::default(),
            }],
        }];
        let mut panel = ModelPanel::new(sources, Some("ds-flash"));
        assert_eq!(panel.committed_at(0), Some(0), "id 匹配预选");
        assert_eq!(
            panel.handle_key(key(KeyCode::Enter)),
            ModelPanelAction::Apply {
                model_id: "ds-flash".into(),
            },
            "值 = id（不是调用名、不是展示名）"
        );
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
        let mut panel = ModelPanel::new(same_name_sources(), Some("only-b"));
        let refreshed = vec![
            group("dashscope", &["shared-a", "only-a", "new-a"]),
            group("dashscope-openai", &["shared-b", "only-b", "new-b"]),
        ];
        panel.set_sources(refreshed);
        assert_eq!(panel.current_page(), 1);
        assert_eq!(panel.cursor(), 1);
        assert_eq!(panel.committed_at(1), Some(1));
    }

    /// 刷新按 **id** 重解析：目录重排后光标与标记跟的是同一个模型（不是同一行号）。
    #[test]
    fn refresh_tracks_the_id_across_reordering() {
        let mut panel = ModelPanel::new(vec![group("p", &["a", "b", "c"])], Some("c"));
        panel.handle_key(key(KeyCode::Up)); // cursor → b
        panel.handle_key(key(KeyCode::Up)); // cursor → a
        assert_eq!(panel.cursor(), 0, "cursor preselected a");
        // 重排 + 插入：a 与 c 都换了行号。
        panel.set_sources(vec![group("p", &["new", "a", "b", "c"])]);
        assert_eq!(panel.cursor(), 1, "cursor follows a's new row");
        assert_eq!(panel.committed_at(0), Some(3), "the mark follows the c id");
    }

    #[test]
    fn refresh_falls_back_when_the_current_page_vanishes() {
        let mut panel = ModelPanel::new(same_name_sources(), None);
        panel.handle_key(key(KeyCode::Right)); // page 1
        panel.set_sources(vec![group("dashscope", &["shared-a"])]);
        assert_eq!(panel.current_page(), 0, "missing provider → first page");
        assert_eq!(panel.cursor(), 0);
    }

    #[test]
    fn refresh_clamps_cursor_and_clears_vanished_mark() {
        let mut panel = ModelPanel::new(vec![group("p", &["a", "b", "c"])], Some("c"));
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

    // ── Window ──────────────────────────────────────────────────

    #[test]
    fn window_on_long_model_list_centers_the_cursor() {
        use crate::shared::panels::PANEL_WINDOW;
        use crate::shared::panels::window_range;
        let many: Vec<ModelDetail> = (0..8)
            .map(|i| detail(&format!("m{i}"), &format!("m{i}")))
            .collect();
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
        assert_eq!(range, 3..8, "cursor (m5) centered in the window");
        assert!(range.contains(&panel.cursor()));
        // Clamped at the end: the last rows fill the window, the cursor moves
        // to the bottom slot instead of wrapping to the top.
        for _ in 0..5 {
            panel.handle_key(key(KeyCode::Down));
        }
        assert_eq!(panel.cursor(), 7, "cursor clamps at the last model");
        let range = window_range(panel.cursor(), panel.models().len(), PANEL_WINDOW);
        assert_eq!(range, 3..8);
    }
}
