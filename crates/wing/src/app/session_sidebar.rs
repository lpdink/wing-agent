//! Session sidebar state, pointer gestures, and the shared session-list request.
//!
//! A picker open can join the in-flight query. Local mutations schedule a
//! follow-up refresh and invalidate its picker result. Connection loss discards
//! the entire old reply. Successful list snapshots belong to the gateway and
//! survive switches between sessions in the same connection.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use tokio::time::Instant;
use wing_api_client::models::SessionListResponse;

use super::mouse::MouseOutcome;
use super::{App, AppIntent};
use crate::config::{SessionSidebarConfig, SidebarSide};
use crate::protocol::SessionStatus;
use crate::shared::session_list::{SessionAge, SessionCandidate, session_forest};
use crate::shared::session_tree::Crew;
use crate::ui::session_sidebar::{self as ui, RowAction, SidebarChrome, SidebarFrame, SidebarRow};
use crate::ui::toast::Toast;

const MIN_MAIN_WIDTH: u16 = 64;
const MIN_TERMINAL_WIDTH: u16 = MIN_MAIN_WIDTH + SessionSidebarConfig::MIN_WIDTH;
const MIN_HEIGHT: u16 = 8;
const WHEEL_SCROLL_SESSIONS: isize = 3;
const ACTIVE_REFRESH: Duration = Duration::from_secs(8);
const IDLE_REFRESH: Duration = Duration::from_secs(30);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

fn split(area: Rect, open: bool, side: SidebarSide, preferred_width: u16) -> (Rect, Option<Rect>) {
    if !open || session_sidebar_too_small(area) {
        return (area, None);
    }
    let width = clamp_sidebar_width(i32::from(preferred_width), area.width);
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

fn clamp_sidebar_width(width: i32, screen_width: u16) -> u16 {
    let max_width = screen_width.saturating_sub(MIN_MAIN_WIDTH).clamp(
        SessionSidebarConfig::MIN_WIDTH,
        SessionSidebarConfig::MAX_WIDTH,
    );
    width.clamp(
        i32::from(SessionSidebarConfig::MIN_WIDTH),
        i32::from(max_width),
    ) as u16
}

struct SidebarResize {
    side: SidebarSide,
    start_column: u16,
    start_width: u16,
    preview_width: u16,
}

impl SidebarResize {
    fn width_at(&self, column: u16, screen_width: u16) -> u16 {
        let dx = i32::from(column) - i32::from(self.start_column);
        let change = match self.side {
            SidebarSide::Left => dx,
            SidebarSide::Right => -dx,
        };
        clamp_sidebar_width(i32::from(self.start_width) + change, screen_width)
    }
}

struct CurrentSession<'a> {
    id: &'a str,
    title: Option<&'a str>,
    workspace: Option<&'a str>,
    status: Option<SessionStatus>,
}

#[derive(Clone, Copy)]
struct TreeRow {
    index: usize,
    group: SessionAge,
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
    open: bool,
    pub frame: SidebarFrame,
    press: Option<(String, RowAction)>,
    resize: Option<SidebarResize>,
    snapshot: Option<Vec<SessionCandidate>>,
    last_fetch: Option<Instant>,
    active_sessions: bool,
    fetch: Option<ListRequest>,
    refresh_requested: bool,
    consecutive_failures: u32,
    expanded: HashSet<String>,
    candidates: Vec<SessionCandidate>,
    rows: Vec<TreeRow>,
    current: Option<SessionCandidate>,
    current_was_synthetic: bool,
    group_day: Option<NaiveDate>,
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

    pub fn layout(&self, area: Rect, config: SessionSidebarConfig) -> (Rect, Option<Rect>) {
        let width = self
            .resize
            .as_ref()
            .map_or(config.width, |drag| drag.preview_width);
        split(area, self.open, config.side, width)
    }

    pub fn is_resizing(&self) -> bool {
        self.resize.is_some()
    }

    pub fn cancel_resize(&mut self) {
        self.resize = None;
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

    pub fn deadline(&self, can_fetch: bool) -> Option<Instant> {
        if !can_fetch || self.frame.area.is_empty() || self.fetch.is_some() {
            return None;
        }
        if self.refresh_requested {
            return Some(Instant::now());
        }
        let interval =
            if self.active_sessions || self.current.as_ref().is_some_and(session_in_flight) {
                ACTIVE_REFRESH
            } else {
                IDLE_REFRESH
            };
        let multiplier = 1 << self.consecutive_failures.min(3);
        let delay = interval.saturating_mul(multiplier).min(MAX_RETRY_DELAY);
        Some(
            self.last_fetch
                .map_or_else(Instant::now, |last| last + delay),
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
            if !self.expanded.is_empty() {
                let ids: HashSet<_> = candidates.iter().map(|row| row.id.as_str()).collect();
                self.expanded.retain(|id| ids.contains(id.as_str()));
            }
            self.active_sessions = candidates.iter().any(session_in_flight);
            self.snapshot = Some(candidates);
            self.tree_dirty = true;
        }
    }

    fn sync_current(&mut self, current: CurrentSession<'_>, area: Rect, today: NaiveDate) {
        let reveal = self.current.as_ref().is_none_or(|row| row.id != current.id);
        if self.current.as_ref().is_none_or(|row| {
            row.id != current.id
                || row.parsed_status() != current.status
                || row.title != current.title.unwrap_or_default()
                || row.workspace != current.workspace.unwrap_or_default()
        }) {
            self.current = Some(SessionCandidate {
                id: current.id.to_owned(),
                parent: None,
                title: current.title.unwrap_or_default().to_owned(),
                workspace: current.workspace.unwrap_or_default().to_owned(),
                status: current
                    .status
                    .map_or_else(String::new, |status| status.as_str().to_owned()),
                last_interaction: String::new(),
                pinned: false,
                pin_added_at: None,
            });
            self.tree_dirty = true;
        }
        if self.tree_dirty || self.group_day != Some(today) {
            self.rebuild(area, today, reveal.then_some(current.id));
            self.tree_dirty = false;
            self.group_day = Some(today);
        }
        self.clamp_offset(area);
    }

    fn rebuild(&mut self, area: Rect, today: NaiveDate, reveal: Option<&str>) {
        let anchor = self
            .rows
            .get(self.offset)
            .map(|row| self.candidates[row.index].id.clone());
        if let Some(snapshot) = &self.snapshot {
            self.candidates.clone_from(snapshot);
        } else {
            self.candidates.clear();
        }
        let mut reveal = reveal;
        let mut synthetic = None;
        if let Some(current) = &self.current {
            if let Some(row) = self.candidates.iter_mut().find(|row| row.id == current.id) {
                if self.current_was_synthetic {
                    reveal = Some(&current.id);
                }
                // An interactive ask can be visible in the list before its panel arrives.
                match (current.parsed_status(), row.parsed_status()) {
                    (None, _) | (Some(SessionStatus::Working), Some(SessionStatus::Waiting)) => {}
                    (Some(_), _) => row.status.clone_from(&current.status),
                }
            } else {
                synthetic = Some(self.candidates.len());
                self.candidates.push(current.clone());
            }
        }
        self.current_was_synthetic = synthetic.is_some();
        let forest = session_forest(&self.candidates);
        if let Some(index) =
            reveal.and_then(|id| self.candidates.iter().position(|row| row.id == id))
        {
            let mut parent = forest.parent_of(index);
            while let Some(index) = parent {
                self.expanded.insert(self.candidates[index].id.clone());
                parent = forest.parent_of(index);
            }
        }

        let mut buckets = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        let mut remaining = forest.tree();
        while let Some(root) = remaining.first() {
            // Crew counts include every descendant, so the preorder slice contains the whole tree.
            let (tree, rest) = remaining.split_at(forest.crew(root.index).descendants + 1);
            remaining = rest;
            let date = if synthetic == Some(root.index) {
                Some(today)
            } else {
                tree.iter()
                    .filter_map(|entry| {
                        local_activity_date(&self.candidates[entry.index].last_interaction)
                    })
                    .max()
            };
            let group = SessionAge::at_date(date, today);
            let mut folded_depth = None;
            for entry in tree {
                if folded_depth.is_some_and(|depth| entry.depth > depth) {
                    continue;
                }
                let crew = forest.crew(entry.index);
                let folded = crew.descendants > 0
                    && !self.expanded.contains(&self.candidates[entry.index].id);
                folded_depth = folded.then_some(entry.depth);
                buckets[group as usize].push(TreeRow {
                    index: entry.index,
                    depth: entry.depth,
                    crew,
                    group,
                });
            }
        }
        self.rows.clear();
        self.rows.extend(buckets.into_iter().flatten());

        if let Some(index) = anchor.as_deref().and_then(|id| self.row_position(id)) {
            self.offset = index;
        }
        if let Some(index) = reveal.and_then(|id| self.row_position(id)) {
            let fitting =
                ui::fitting_rows(area, self.rows[..=index].iter().rev().map(|row| row.group));
            let earliest = (index + 1).saturating_sub(fitting).min(index);
            self.offset = self.offset.clamp(earliest, index);
        }
    }

    fn row_position(&self, id: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| self.candidates[row.index].id == id)
    }

    fn clamp_offset(&mut self, area: Rect) {
        let final_page = ui::fitting_rows(area, self.rows.iter().rev().map(|row| row.group));
        self.offset = self.offset.min(self.rows.len().saturating_sub(final_page));
    }

    fn toggle_children(&mut self, id: String) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.tree_dirty = true;
    }

    pub fn scroll(&mut self, down: bool) -> MouseOutcome {
        let previous = self.offset;
        let step = if down {
            WHEEL_SCROLL_SESSIONS
        } else {
            -WHEEL_SCROLL_SESSIONS
        };
        self.offset = self.offset.saturating_add_signed(step);
        self.clamp_offset(self.frame.area);
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
        self.session_sidebar.cancel_resize();
        if !self.session_sidebar.open {
            self.session_sidebar.frame = SidebarFrame::default();
        }
        if !session_sidebar_too_small(self.geometry.area()) {
            // Inline terminal graphics may still cover cells reassigned to the side column.
            self.needs_full_redraw |= self.images.is_enabled();
        } else if self.session_sidebar.open {
            self.show_toast(Toast::info(
                format!("会话侧栏需要 {MIN_TERMINAL_WIDTH} 列 × {MIN_HEIGHT} 行"),
                Duration::from_secs(3),
            ));
        }
    }

    pub(super) fn draw_session_sidebar(
        &mut self,
        area: Option<Rect>,
        side: SidebarSide,
        buf: &mut Buffer,
    ) {
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
            None
        } else if !self.ask_panels.is_empty() {
            Some(SessionStatus::Waiting)
        } else if self.turn.working {
            Some(SessionStatus::Working)
        } else {
            Some(SessionStatus::Idle)
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
            area,
            Local::now().date_naive(),
        );
        let capacity = ui::fitting_rows(
            area,
            sidebar
                .rows
                .iter()
                .skip(sidebar.offset)
                .map(|row| row.group),
        );
        let total = sidebar.rows.len();
        let range = (total > capacity && capacity > 0).then(|| {
            let first = sidebar.offset + 1;
            let last = (sidebar.offset + capacity).min(total);
            format!("{first}–{last}/{total}")
        });
        let message = if !self.connected {
            Some("离线")
        } else if sidebar.consecutive_failures > 0 {
            Some("更新失败")
        } else if sidebar.snapshot.is_none() {
            Some("加载中…")
        } else {
            range.as_deref()
        };
        sidebar.frame = ui::render(
            area,
            buf,
            SidebarChrome {
                side,
                resizing: sidebar.is_resizing(),
                palette: &self.palette,
                current_id: &self.session_id,
                message,
            },
            sidebar.rows.iter().skip(sidebar.offset).map(|row| {
                let candidate = &sidebar.candidates[row.index];
                SidebarRow {
                    candidate,
                    group: row.group,
                    depth: row.depth,
                    crew: row.crew,
                    collapsed: row.crew.descendants > 0
                        && !sidebar.expanded.contains(&candidate.id),
                }
            }),
        );
    }

    pub(super) fn sidebar_press(&mut self, position: Position) -> MouseOutcome {
        self.cancel_selection();
        let sidebar = &mut self.session_sidebar;
        if sidebar.frame.divider.contains(position) {
            sidebar.resize = Some(SidebarResize {
                side: self.config.layout.session_sidebar.side,
                start_column: position.x,
                start_width: sidebar.frame.area.width,
                preview_width: sidebar.frame.area.width,
            });
            self.needs_full_redraw |= self.images.is_enabled();
            return MouseOutcome::Immediate;
        }
        sidebar.press = sidebar
            .frame
            .hit(position)
            .map(|(id, action)| (id.to_owned(), action));
        MouseOutcome::Immediate
    }

    pub(super) fn resize_sidebar_to(&mut self, column: u16) -> Option<MouseOutcome> {
        let drag = self.session_sidebar.resize.as_mut()?;
        let width = drag.width_at(column, self.geometry.width());
        if width == drag.preview_width {
            return Some(MouseOutcome::Ignored);
        }
        drag.preview_width = width;
        Some(MouseOutcome::Coalesced)
    }

    pub(super) fn sidebar_release(&mut self, position: Position) -> Option<MouseOutcome> {
        if let Some(drag) = self.session_sidebar.resize.take() {
            if !self.background_pointer_blocked() {
                let width = drag.width_at(position.x, self.geometry.width());
                if width != drag.start_width {
                    self.config.layout.session_sidebar.width = width;
                    self.push_intent(AppIntent::SaveSidebarWidth { width });
                }
            }
            return Some(MouseOutcome::Immediate);
        }
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
            tracing::debug!("ignoring session list result without an in-flight request");
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
                self.session_sidebar.consecutive_failures = 0;
            }
            Err(error) => {
                tracing::warn!(%error, "list sessions failed");
                self.session_sidebar.consecutive_failures =
                    self.session_sidebar.consecutive_failures.saturating_add(1);
            }
        }
    }
}

fn session_sidebar_too_small(area: Rect) -> bool {
    area.width < MIN_TERMINAL_WIDTH || area.height < MIN_HEIGHT
}

fn session_in_flight(row: &SessionCandidate) -> bool {
    row.parsed_status()
        .is_some_and(SessionStatus::turn_in_flight)
}

fn local_activity_date(stamp: &str) -> Option<NaiveDate> {
    if let Ok(naive) = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.date());
    }
    DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Local).date_naive())
}
