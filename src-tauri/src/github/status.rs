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
    #[serde(default)]
    pub review_decision: ReviewDecision,
    pub unresolved_threads: u32,
    pub head_sha: String,
    pub fetched_at: DateTime<Utc>,
    #[serde(default)]
    pub last_error: Option<String>,
}
