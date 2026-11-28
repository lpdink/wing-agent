//! Session list facts shared by the TUI session picker and sidebar.

use wing_api_client::models::SessionInfo;

use crate::protocol::SessionStatus;
use crate::shared::pinning::{is_pinned, pin_added_at};
use crate::shared::session_tree::{Forest, TreeRow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCandidate {
    /// Stable session identity.
    pub id: String,
    /// 创建者会话 id（`parent_session_id`；根 = `None`）。
    pub parent: Option<String>,
    /// Session title.
    pub title: String,
    /// Session workspace.
    pub workspace: String,
    /// Runtime status string from the list (inactive, idle, working, waiting).
    pub status: String,
    /// This session's own last interaction timestamp (ISO 8601).
    pub last_interaction: String,
    /// Whether the session carries the `pin` tag.
    pub pinned: bool,
    /// Timestamp from `tag_meta.pin.added_at`.
    pub pin_added_at: Option<String>,
}

impl From<SessionInfo> for SessionCandidate {
    fn from(info: SessionInfo) -> Self {
        Self {
            id: info.id,
            parent: info.parent_session_id,
            title: info.name.unwrap_or_default(),
            workspace: info.workspace.unwrap_or_default(),
            status: info.status,
            last_interaction: info.last_interaction.unwrap_or_default(),
            pinned: is_pinned(&info.tags),
            pin_added_at: pin_added_at(&info.tag_meta).map(str::to_owned),
        }
    }
}

pub fn session_forest(candidates: &[SessionCandidate]) -> Forest<'_> {
    let rows: Vec<_> = candidates
        .iter()
        .map(|candidate| TreeRow {
            id: &candidate.id,
            parent: candidate.parent.as_deref(),
            pinned: candidate.pinned,
            pin_added_at: candidate.pin_added_at.as_deref(),
            last_interaction: Some(&candidate.last_interaction),
            status: SessionStatus::parse(&candidate.status),
        })
        .collect();
    Forest::build(&rows)
}
