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

export type JobEvent =
  | { kind: "status"; message: string; repo?: string }
  | { kind: "log"; stream: "stdout" | "stderr"; line: string; repo?: string }
  | { kind: "success" }
  | { kind: "failed"; error: string };

export interface OrphanedDir {
  path: string;
}

export interface MissingWorktree {
  workspace_id: string;
  branch: string;
  repo_key: string;
  worktree_path: string;
}

export interface Discrepancies {
  orphaned_dirs: OrphanedDir[];
  missing_worktrees: MissingWorktree[];
}

export interface SessionInfo {
  id: string;
  workspace_id: string;
  repo_key: string;
  cwd: string;
  running: boolean;
  runtime_state: SessionRuntimeState;
  notification_type: string | null;
}

export interface TurnChangedEvent {
  workspace_id: string;
  session_id: string;
  runtime_state: SessionRuntimeState;
  notification_type: string | null;
}

export interface ThemeColors {
  background: string;
  foreground: string;
  cursor: string;
  cursor_text: string;
  selection: string;
  /** 16 ANSI colors, `ansi[0]` = black, `ansi[1]` = red, etc. */
  ansi: string[];
}

export interface Theme {
  name: string;
  source_path: string;
  colors: ThemeColors;
}
