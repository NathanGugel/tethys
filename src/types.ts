export type WorkspaceId = string;
export type SessionId = string;

export type SessionRuntimeState =
  | "dormant"
  | "working"
  | "waiting_input"
  | "idle";

export interface RepoLink {
  repo_key: string;
  worktree_path: string;
  setup_script_ran_at: string | null;
}

export interface ClaudeSessionMeta {
  id: SessionId;
  repo_key: string;
  cwd: string;
  claude_session_id: string | null;
  transcript_path: string | null;
}

export interface Workspace {
  id: WorkspaceId;
  branch: string;
  paused: boolean;
  created_at: string;
  repo_links: RepoLink[];
  sessions: ClaudeSessionMeta[];
}

export interface CreateWorkspaceArgs {
  branch: string;
  repo_selections: string[];
}

export interface Repo {
  key: string;
  remote_url: string;
  default_setup_script: string | null;
  setup_timeout_secs: number | null;
}

export type RegistryStatus =
  | { kind: "ok"; path: string; registry: { worktree_root: string; repos: Repo[] } }
  | { kind: "missing"; path: string }
  | { kind: "invalid"; path: string; error: string };
