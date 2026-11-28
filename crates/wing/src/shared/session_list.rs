//! Session list facts shared by the TUI session picker and sidebar.

use chrono::NaiveDate;
use wing_api_client::models::SessionInfo;

use crate::protocol::SessionStatus;
use crate::shared::pinning::{is_pinned, pin_added_at};
use crate::shared::session_tree::{Forest, TreeRow};

/// Sidebar section order, measured in local calendar days.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SessionAge {
    Recent,
    Yesterday,
    Week,
    Older,
}

impl SessionAge {
    /// Future dates share today’s section. Unknown dates belong to the oldest section.
    pub(crate) fn of(activity: Option<NaiveDate>, today: NaiveDate) -> Self {
        let Some(date) = activity else {
            return Self::Older;
        };
        match today.signed_duration_since(date).num_days() {
            ..=0 => Self::Recent,
            1 => Self::Yesterday,
            2..=7 => Self::Week,
            _ => Self::Older,
        }
    }
}

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

impl SessionCandidate {
    pub fn parsed_status(&self) -> Option<SessionStatus> {
        SessionStatus::parse(&self.status)
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
            status: candidate.parsed_status(),
        })
        .collect();
    Forest::build(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_buckets_use_calendar_days_across_a_year_boundary() {
        let today = NaiveDate::from_ymd_opt(2027, 1, 2).unwrap();
        for (stamp, expected) in [
            ("2027-01-03", SessionAge::Recent),
            ("2027-01-02", SessionAge::Recent),
            ("2027-01-01", SessionAge::Yesterday),
            ("2026-12-31", SessionAge::Week),
            ("2026-12-26", SessionAge::Week),
            ("2026-12-25", SessionAge::Older),
        ] {
            let activity = NaiveDate::parse_from_str(stamp, "%Y-%m-%d").unwrap();
            assert_eq!(SessionAge::of(Some(activity), today), expected);
        }
        assert_eq!(SessionAge::of(None, today), SessionAge::Older);
    }
}
