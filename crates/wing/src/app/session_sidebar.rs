//! Session sidebar state, pointer gestures, and the shared session-list request.

use std::collections::HashSet;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use tokio::time::Instant;
use wing_api_client::models::SessionListResponse;

use super::mouse::MouseOutcome;
use super::{App, AppIntent};
use crate::config::SidebarSide;
use crate::shared::session_list::{SessionCandidate, session_forest};
use crate::shared::session_tree::Crew;
use crate::ui::session_sidebar::{self as ui, RowAction, SidebarFrame, SidebarRow};
use crate::ui::toast::Toast;

const MIN_WIDTH: u16 = 100;
const MIN_HEIGHT: u16 = 8;
const ACTIVE_REFRESH: Duration = Duration::from_secs(8);
const IDLE_REFRESH: Duration = Duration::from_secs(30);

pub(super) fn split(area: Rect, open: bool, side: SidebarSide) -> (Rect, Option<Rect>) {
    if !open || session_sidebar_too_small(area) {
        return (area, None);
    }
    let width = ((u32::from(area.width) * 28 / 100) as u16).clamp(30, 40);
    let mut main = Rect {
        width: area.width - width,
        ..area
    };
    let mut sidebar = Rect::new(area.x, area.y + 1, width, area.height - 1);
    match side {
        SidebarSide::Left => main.x += width,
        SidebarSide::Right => sidebar.x = main.right(),
    }
    (main, Some(sidebar))
}

struct CurrentSession<'a> {
    id: &'a str,
    title: Option<&'a str>,
    workspace: Option<&'a str>,
    status: &'static str,
}

#[derive(Clone, Copy)]
struct TreeRow {
    index: usize,
    depth: usize,
    crew: Crew,
}

#[derive(Default)]
struct ListRequest {
    popup_session: Option<String>,
    disconnected: bool,
    invalidated: bool,
}

#[derive(Default)]
pub(super) struct SessionSidebar {
    pub open: bool,
    pub frame: SidebarFrame,
    press: Option<(String, RowAction)>,
    snapshot: Option<Vec<SessionCandidate>>,
    last_fetch: Option<Instant>,
    active_sessions: bool,
    fetch: Option<ListRequest>,
    refresh_requested: bool,
    failed: bool,
    collapsed: HashSet<String>,
    candidates: Vec<SessionCandidate>,
    rows: Vec<TreeRow>,
    current: Option<SessionCandidate>,
    tree_dirty: bool,
    offset: usize,
}

impl SessionSidebar {
    pub fn new(open: bool) -> Self {
        Self {
            open,
            ..Self::default()
        }
    }

    pub fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    pub fn invalidate_list(&mut self) {
        self.request_refresh();
        if let Some(fetch) = &mut self.fetch {
            fetch.invalidated = true;
        }
    }

    pub fn cancel_press(&mut self) {
        self.press = None;
    }

    pub fn connection_changed(&mut self, connected: bool) {
        self.request_refresh();
        self.cancel_press();
        if !connected && let Some(fetch) = &mut self.fetch {
            fetch.disconnected = true;
        }
    }

    fn current_status(&self) -> Option<&str> {
        self.current.as_ref().map(|row| row.status.as_str())
    }

    pub fn deadline(&self, connected: bool) -> Option<Instant> {
        if !connected || self.frame.area.is_empty() || self.fetch.is_some() {
            return None;
        }
        if self.refresh_requested {
            return Some(Instant::now());
        }
        let interval = if self.active_sessions || self.current_status().is_some_and(is_active) {
            ACTIVE_REFRESH
        } else {
            IDLE_REFRESH
        };
        Some(
            self.last_fetch
                .map_or_else(Instant::now, |last| last + interval),
        )
    }

    pub fn begin_fetch(&mut self, popup_session: Option<&str>) -> bool {
        if let Some(fetch) = &mut self.fetch {
            if let Some(session_id) = popup_session {
                fetch.popup_session = Some(session_id.to_owned());
            }
            return false;
        }
        self.fetch = Some(ListRequest {
            popup_session: popup_session.map(str::to_owned),
            ..ListRequest::default()
        });
        self.refresh_requested = false;
        true
    }

    fn set_snapshot(&mut self, candidates: Vec<SessionCandidate>) {
        if self.snapshot.as_ref() != Some(&candidates) {
            if !self.collapsed.is_empty() {
                let ids: HashSet<_> = candidates.iter().map(|row| row.id.as_str()).collect();
                self.collapsed.retain(|id| ids.contains(id.as_str()));
            }
            self.active_sessions = candidates.iter().any(|row| is_active(&row.status));
            self.snapshot = Some(candidates);
            self.tree_dirty = true;
        }
    }

    fn sync_current(&mut self, current: CurrentSession<'_>, page: usize) {
        let reveal = self.current.as_ref().is_none_or(|row| row.id != current.id);
        if self.current.as_ref().is_none_or(|row| {
            row.id != current.id
                || row.status != current.status
                || row.title != current.title.unwrap_or_default()
                || row.workspace != current.workspace.unwrap_or_default()
        }) {
            self.current = Some(SessionCandidate {
                id: current.id.to_owned(),
                parent: None,
                title: current.title.unwrap_or_default().to_owned(),
                workspace: current.workspace.unwrap_or_default().to_owned(),
                status: current.status.to_owned(),
                last_interaction: String::new(),
                pinned: false,
                pin_added_at: None,
            });
            self.tree_dirty = true;
        }
        if self.tree_dirty {
            self.rebuild(page, reveal.then_some(current.id));
            self.tree_dirty = false;
        }
        self.clamp_offset(page);
    }

    fn rebuild(&mut self, page: usize, reveal: Option<&str>) {
        let anchor = self
            .rows
            .get(self.offset)
            .map(|row| self.candidates[row.index].id.clone());
        if let Some(snapshot) = &self.snapshot {
            self.candidates.clone_from(snapshot);
        } else {
            self.candidates.clear();
        }
        if let Some(current) = &self.current {
            if let Some(row) = self.candidates.iter_mut().find(|row| row.id == current.id) {
                if !current.status.is_empty()
                    && (current.status != "working" || row.status != "waiting")
                {
                    row.status.clone_from(&current.status);
                }
            } else {
                self.candidates.push(current.clone());
            }
        }
        let forest = session_forest(&self.candidates);
        if let Some(index) =
            reveal.and_then(|id| self.candidates.iter().position(|row| row.id == id))
        {
            let mut parent = forest.parent_of(index);
            while let Some(index) = parent {
                self.collapsed.remove(&self.candidates[index].id);
                parent = forest.parent_of(index);
            }
        }

        self.rows.clear();
        let mut folded_depth = None;
        for entry in forest.tree() {
            if folded_depth.is_some_and(|depth| entry.depth > depth) {
                continue;
            }
            folded_depth = self
                .collapsed
                .contains(&self.candidates[entry.index].id)
                .then_some(entry.depth);
            self.rows.push(TreeRow {
                index: entry.index,
                depth: entry.depth,
                crew: forest.crew(entry.index),
            });
        }
        if let Some(index) = anchor.as_deref().and_then(|id| self.row_position(id)) {
            self.offset = index;
        }
        if let Some(index) = reveal.and_then(|id| self.row_position(id)) {
            self.offset = self
                .offset
                .clamp(index.saturating_add(1).saturating_sub(page), index);
        }
    }

    fn row_position(&self, id: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| self.candidates[row.index].id == id)
    }

    fn clamp_offset(&mut self, page: usize) {
        self.offset = self.offset.min(self.rows.len().saturating_sub(page));
    }

    fn toggle_children(&mut self, id: String) {
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        self.tree_dirty = true;
    }

    pub fn scroll(&mut self, down: bool) -> MouseOutcome {
        self.cancel_press();
        let previous = self.offset;
        self.offset = self.offset.saturating_add_signed(if down { 3 } else { -3 });
        self.clamp_offset(ui::capacity(self.frame.area));
        if previous != self.offset {
            MouseOutcome::Immediate
        } else {
            MouseOutcome::Ignored
        }
    }
}

impl App {
    pub(super) fn toggle_session_sidebar(&mut self) {
        self.session_sidebar.open = !self.session_sidebar.open;
        self.session_sidebar.cancel_press();
        if !session_sidebar_too_small(self.geometry.area()) {
            self.needs_full_redraw = true;
        }
        if self.session_sidebar.open
            && self.session_sidebar.frame.area.is_empty()
            && session_sidebar_too_small(self.geometry.area())
        {
            self.show_toast(Toast::info(
                format!("会话侧栏需要 {MIN_WIDTH} 列 × {MIN_HEIGHT} 行"),
                Duration::from_secs(3),
            ));
        }
    }

    pub(super) fn draw_session_sidebar(&mut self, area: Option<Rect>, buf: &mut Buffer) {
        let sidebar = &mut self.session_sidebar;
        if sidebar.frame.area != area.unwrap_or_default() {
            sidebar.cancel_press();
        }
        let Some(area) = area else {
            sidebar.frame = SidebarFrame::default();
            return;
        };
        if sidebar.frame.area.is_empty() {
            sidebar.request_refresh();
        }
        let status = if self
            .pending_session_snapshot
            .as_ref()
            .is_some_and(|pending| pending.session_id == self.session_id)
        {
            ""
        } else if !self.ask_panels.is_empty() {
            "waiting"
        } else if self.turn.working {
            "working"
        } else {
            "idle"
        };
        sidebar.sync_current(
            CurrentSession {
                id: &self.session_id,
                title: self
                    .status
                    .session_name
                    .as_deref()
                    .filter(|title| !title.is_empty()),
                workspace: self.status.workdir.as_deref(),
                status,
            },
            ui::capacity(area),
        );
        let message = if !self.connected {
            Some("离线")
        } else if sidebar.failed {
            Some("更新失败")
        } else if sidebar.snapshot.is_none() {
            Some("加载中…")
        } else {
            None
        };
        sidebar.frame = ui::render(
            area,
            buf,
            self.config.layout.session_sidebar.side,
            &self.palette,
            &self.session_id,
            message,
            sidebar.rows.iter().skip(sidebar.offset).map(|row| {
                let candidate = &sidebar.candidates[row.index];
                SidebarRow {
                    candidate,
                    depth: row.depth,
                    crew: row.crew,
                    collapsed: sidebar.collapsed.contains(&candidate.id),
                }
            }),
        );
    }

    pub(super) fn sidebar_press(&mut self, position: Position) -> MouseOutcome {
        self.cancel_selection();
        let sidebar = &mut self.session_sidebar;
        sidebar.press = sidebar
            .frame
            .hit(position)
            .map(|(id, action)| (id.to_owned(), action));
        MouseOutcome::Immediate
    }

    pub(super) fn sidebar_release(&mut self, position: Position) -> Option<MouseOutcome> {
        let (id, action) = self.session_sidebar.press.take()?;
        if self.background_pointer_blocked()
            || self.session_sidebar.frame.hit(position) != Some((id.as_str(), action))
        {
            return Some(MouseOutcome::Ignored);
        }
        match action {
            RowAction::ToggleChildren => self.session_sidebar.toggle_children(id),
            RowAction::Open if id != self.session_id && self.user_input_ready() => {
                self.push_intent(AppIntent::ResumeSession { session_id: id });
            }
            RowAction::Open => return Some(MouseOutcome::Ignored),
        }
        Some(MouseOutcome::Immediate)
    }

    pub(super) fn finish_session_list(&mut self, result: Result<SessionListResponse, String>) {
        let Some(request) = self.session_sidebar.fetch.take() else {
            return;
        };
        let popup = request.popup_session.as_deref() == Some(self.session_id.as_str())
            && self.subscription_target() == self.session_id;
        if request.disconnected {
            if popup && self.connected {
                self.push_intent(AppIntent::FetchSessionList);
            }
            return;
        }
        if popup && request.invalidated {
            self.push_intent(AppIntent::FetchSessionList);
        }
        self.session_sidebar.last_fetch = Some(Instant::now());
        match result {
            Ok(response) => {
                let candidates: Vec<_> = response
                    .sessions
                    .into_iter()
                    .map(SessionCandidate::from)
                    .collect();
                if popup && !request.invalidated {
                    self.apply_popup_sessions(&candidates);
                }
                self.session_sidebar.set_snapshot(candidates);
                self.session_sidebar.failed = false;
            }
            Err(error) => {
                tracing::warn!(%error, "list sessions failed");
                self.session_sidebar.failed = true;
            }
        }
    }
}

fn session_sidebar_too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}

fn is_active(status: &str) -> bool {
    matches!(status, "working" | "waiting")
}
