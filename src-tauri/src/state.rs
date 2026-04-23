use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeSessionMeta {
    pub id: SessionId,
    pub repo_key: String,
    pub cwd: PathBuf,
    pub claude_session_id: Option<String>,
    pub transcript_path: Option<PathBuf>,

    #[serde(skip)]
    pub pid: Option<u32>,
    #[serde(skip)]
    pub runtime_state: SessionRuntimeState,
    #[serde(skip)]
    pub last_turn_change_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimeState {
    #[default]
    Dormant,
    Working,
    WaitingInput,
    Idle,
}

impl AppState {
    pub fn find_workspace(&self, id: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn find_workspace_mut(&mut self, id: &str) -> Option<&mut Workspace> {
        self.workspaces.iter_mut().find(|w| w.id == id)
    }
}
