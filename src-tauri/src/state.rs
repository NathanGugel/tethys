use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::github::GithubPrStatus;

pub type WorkspaceId = String;
pub type SessionId = String;

pub fn new_workspace_id() -> WorkspaceId {
    Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub branch: String,
    #[serde(default)]
    pub paused: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub repo_links: Vec<RepoLink>,
    #[serde(default)]
    pub sessions: Vec<ClaudeSessionMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoLink {
    pub repo_key: String,
    pub worktree_path: PathBuf,
    pub setup_script_ran_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub github: Option<GithubPrStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeSessionMeta {
    pub id: SessionId,
    pub repo_key: String,
    pub cwd: PathBuf,
    pub claude_session_id: Option<String>,
    pub transcript_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimeState {
    /// PTY not running (session has never been spawned, or was spawned and exited).
    #[default]
    Dormant,
    /// PTY running and actively processing (Claude is thinking, or user just typed).
    Working,
    /// Claude finished responding, no explicit input prompt up — default "nothing pending" state.
    Idle,
    /// Claude is blocked on user input — either the main prompt or a permission dialog.
    WaitingInput,
}

impl AppState {
    pub fn find_workspace(&self, id: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn find_workspace_mut(&mut self, id: &str) -> Option<&mut Workspace> {
        self.workspaces.iter_mut().find(|w| w.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_github_state_json_round_trips() {
        // This is the shape of state.json from before the `github` field was
        // added to RepoLink. It must still deserialize cleanly.
        let raw = r#"{
            "workspaces": [
                {
                    "id": "abc-123",
                    "branch": "feat/foo",
                    "created_at": "2026-04-01T12:00:00Z",
                    "repo_links": [
                        {
                            "repo_key": "frontend",
                            "worktree_path": "/tmp/wt/abc-123/frontend",
                            "setup_script_ran_at": null
                        }
                    ]
                }
            ]
        }"#;

        let parsed: AppState = serde_json::from_str(raw).expect("old state.json must deserialize");
        assert_eq!(parsed.workspaces.len(), 1);
        let ws = &parsed.workspaces[0];
        assert_eq!(ws.id, "abc-123");
        assert_eq!(ws.branch, "feat/foo");
        assert_eq!(ws.repo_links.len(), 1);
        assert!(ws.repo_links[0].github.is_none());
    }
}
