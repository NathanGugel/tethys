use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksRollup {
    /// No checks configured for the head commit.
    None,
    Pending,
    Success,
    Failure,
    /// GitHub returns NEUTRAL/SKIPPED rollups — treat them as "not failing".
    Neutral,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// No reviews yet, or not applicable (non-Open PR).
    #[default]
    None,
    Approved,
    ChangesRequested,
    /// Required reviewers haven't weighed in yet.
    ReviewRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubPrStatus {
    pub pr_number: u32,
    pub url: String,
    pub state: PrState,
    pub is_draft: bool,
    pub checks: ChecksRollup,
    /// Status of the Cursor Bugbot check, if present. Tracked separately from
    /// the rest of the CI rollup so the UI can show it as its own indicator.
    #[serde(default = "default_bugbot")]
    pub bugbot: ChecksRollup,
    /// True when GitHub reports the PR as having merge conflicts with its
    /// base branch (`mergeable: CONFLICTING`). The UI surfaces this through
    /// the same CI indicator since you can't merge regardless of CI state.
    #[serde(default)]
    pub has_merge_conflicts: bool,
    #[serde(default)]
    pub review_decision: ReviewDecision,
    pub unresolved_threads: u32,
    pub head_sha: String,
    /// Branch the PR is opened from. Needed to tell whether a PR belongs to a
    /// worktree that's already checked out on it, and to know what to check
    /// out when giving a PR its own worktree. `None` for statuses polled
    /// before this field existed — they refresh on the next tick.
    #[serde(default)]
    pub head_branch: Option<String>,
    pub fetched_at: DateTime<Utc>,
    #[serde(default)]
    pub last_error: Option<String>,
}

fn default_bugbot() -> ChecksRollup {
    ChecksRollup::None
}
