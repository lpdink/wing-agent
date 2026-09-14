//! `GoalRole` — the goal display vocabulary shared by the orchestration and the UI.
//!
//! The role is *what the user sees*: the status bar label, the working verb and
//! the separator cell's icon. The goal **state machine** (`GoalPhase` /
//! `GoalAction` / `GoalState`) stays App-side in `app/goal.rs` — read the two
//! together to follow the whole Goal path (vocabulary here, decisions there,
//! effects in `app/goal_lane.rs`).

/// Which agent is involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalRole {
    Executor,
    Checker,
}

impl GoalRole {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Executor => "Executor",
            Self::Checker => "Checker",
        }
    }

    pub fn working_verb(&self) -> &'static str {
        match self {
            Self::Executor => "working",
            Self::Checker => "reviewing",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Self::Executor => "⚡",
            Self::Checker => "🔍",
        }
    }
}
