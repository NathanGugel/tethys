# Tethys — MVP Plan

High-level architectural plan. Once we agree on this, we'll break it into a tactical task list.

---

## MVP scope

A desktop app that lets a single user run and juggle multiple Claude Code sessions across multiple git worktrees, with visibility into which ones need attention.

**In scope:**

- Create a workspace from a set of pre-registered repos; each gets its own git worktree and setup script.
- Start / find / resume Claude Code sessions inside a workspace's worktrees.
- Render Claude Code's TUI interactively (xterm.js) and keep sessions running while the UI isn't focused on them.
- Flag workspaces when Claude is waiting for input or has finished responding ("your turn").
- Pause a workspace so it stops flagging until revived.
- Quick-open VS Code for any repo in the worktree.
- Show PR status (via `gh`) for PRs attached to a workspace.
- Delete a workspace — cleans up worktrees and session state.

**Out of scope for v1:**

- Multi-machine sync / cloud state.
- Creating PRs from inside Tethys.
- Merge / rebase / conflict UI.
- Editing `.claude/settings.json` beyond Tethys-managed hooks.
- Per-workspace env var management (inherits shell env for MVP).
- Multi-user support.

---

## Stack

- **Tauri 2.x** shell.
- **Rust core** (`src-tauri/`): state store, PTY supervision, git/gh shell-outs, hook event ingestion, filesystem watching.
- **TypeScript + React** frontend (`src/`): workspace list, session tabs, xterm.js hosts.
- **xterm.js** with the WebGL addon for the terminal surface; `fit` and `clipboard` addons.
- **`portable-pty`** (Rust) for PTY spawning; one PTY per Claude session.
- **In-memory state + JSON file** for persistence. One `state.json` in the app data dir; in-memory `AppState` is the source of truth, flushed atomically (temp file + `fsync` + `rename`) on a debounced writer.
- **Shell-outs** to `git` and `gh` for MVP (rather than libgit2 / Octokit). Simpler, matches what the user types.

---

## Architecture

### Process model

One Tauri app, two halves connected by Tauri commands + events:

```
┌─────────────────────────────┐        ┌──────────────────────────────┐
│  Frontend (WebView)         │        │  Rust core                   │
│  - React UI                 │ cmds → │  - SessionSupervisor (PTYs)  │
│  - xterm.js per session     │ ← evts │  - AppState (mem + state.json)│
│  - PR / turn indicators     │        │  - GitOps (worktree CLI)     │
│                             │        │  - HookBridge (fifo watcher) │
│                             │        │  - PrPoller (gh CLI)         │
└─────────────────────────────┘        └──────────────────────────────┘
```

All long-lived state lives in Rust. The frontend is a view + input layer; if it's closed or reloaded, sessions keep running.

### IPC surface

**Commands (JS → Rust):**

- `list_workspaces`, `get_workspace(id)`
- `create_workspace({ name, repo_selections, branch })`
- `delete_workspace(id, { force })`
- `pause_workspace(id)` / `resume_workspace(id)`
- `start_claude_session({ workspace_id, repo_key })`
- `resume_claude_session({ workspace_id, repo_key, claude_session_id? })`
- `send_input({ session_id, bytes })`
- `resize({ session_id, cols, rows })`
- `attach_session(session_id)` → returns recent scrollback for UI mount
- `open_vscode({ workspace_id, repo_key })`
- `attach_pr({ workspace_id, repo_key, pr_number })` / `detach_pr(...)`
- `refresh_pr_status(workspace_id)`

**Events (Rust → JS):**

- `pty:data` `{ session_id, bytes }` — batched ~every 16ms.
- `pty:exit` `{ session_id, code }`
- `session:turn_changed` `{ session_id, state: "idle" | "waiting_input" | "working" }`
- `workspace:changed` `{ workspace_id }`
- `pr:changed` `{ workspace_id, pr_number }`

### Data model

Two separate pieces of state:

1. **`RepoRegistry`** — user-edited TOML at `<data_dir>/repos.toml`. Loaded on boot, never written by Tethys.
2. **`AppState`** — Tethys-managed, persisted to `<data_dir>/state.json`.

#### Repo registry (`repos.toml`, user-edited)

```toml
[[repo]]
key = "frontend"
display_name = "Frontend"
origin_path = "/Users/ryan/code/frontend"
default_setup_script = "pnpm install"

[[repo]]
key = "backend"
display_name = "Backend"
origin_path = "/Users/ryan/code/backend"
default_setup_script = "uv sync"
```

Deserialized into a read-only `RepoRegistry` held alongside `AppState`. For MVP, edits require an app restart; a file-watcher can come later.

#### AppState (`state.json`, Tethys-managed)

`AppState` is a Rust struct behind a `RwLock`, serialized to `state.json` by a debounced writer. Sketch:

```rust
struct AppState {
    workspaces: Vec<Workspace>,
}

struct Workspace {
    id, name, branch, paused, created_at,
    repo_links: Vec<RepoLink>,
    sessions:   Vec<ClaudeSessionMeta>,   // durable metadata only
    pr_links:   Vec<PrLink>,
}

struct RepoLink { repo_key, worktree_path, setup_script_ran_at }

struct ClaudeSessionMeta {
    id, repo_key, cwd, claude_session_id,
    // ephemeral, not persisted:
    #[serde(skip)] pid, state, last_turn_change_at,
}

struct PrLink { repo_key, pr_number, status_json, fetched_at }
```

`branch` lives on `Workspace` (not `RepoLink`) because a workspace uses a single branch name across every repo it spans — chosen by the user at creation time.

**What's persisted vs ephemeral:**

- Persisted: workspaces, repo_links, pr_links, session metadata (id, cwd, claude_session_id).
- Ephemeral (in-memory only, not written): session `pid` / `state` / `last_turn_change_at`, PTY ring buffers.

Schema evolution uses `#[serde(default)]` on new fields — no migrations for MVP.

### Key subsystems

**AppState / Store.** `RwLock<AppState>` in a shared `Arc`. All mutations go through a `store.mutate(|s| { ... })` helper that (a) applies the change, (b) emits a `workspace:changed` event, (c) schedules a debounced flush (~250ms) to `state.json`. Writer serializes to `state.json.tmp`, `fsync`s, `rename`s. Losing the last <1s of writes on a hard crash is acceptable; nothing we persist is unrecoverable.

**GitOps.** Shells out to `git worktree add/remove/list`. On workspace create: one worktree per selected repo, in a Tethys-managed dir (e.g. `~/Library/Application Support/tethys/worktrees/<workspace>/<repo>`). Runs the repo's setup script after worktree creation. On delete: stops sessions, then `git worktree remove`; prompts for `--force` if dirty.

*Branch naming:* workspace creation takes a single `branch` name (user-supplied in the create dialog) that's used for every repo in the workspace: `git worktree add <path> -b <branch>`. No templating, no per-repo overrides in v1.

*Setup script failures:* if any repo's setup script exits non-zero, workspace creation is **blocked** — we tear down the worktrees we already created for the workspace and surface the script's stdout/stderr to the UI. The user fixes the problem and retries from a clean slate. No partially-created workspaces.

**SessionSupervisor.** Owns `portable-pty` children. Each Claude session is one PTY running `claude` (or `claude --resume <id>`) with `cwd=worktree_path`. Reads stdout into a ring buffer (for late-attaching UIs) and batches into `pty:data` events. Writes to stdin on `send_input`. On process exit, emits `pty:exit` and updates `AppState`.

*Across app restarts:* on app quit, Tethys kills its PTYs — the `claude` processes terminate with them. `claude_session_id` is persisted in `state.json`, so on next launch the UI shows each previous session as dormant-but-resumable; clicking it spawns a new PTY running `claude --resume <id>` in the same worktree. We lean entirely on Claude Code's own session resumption — no daemon, no detached PTYs, no state we have to reconstruct ourselves.

**HookBridge — the "your turn" mechanism.**

Tethys installs two hooks into `~/.claude/settings.json` on first run (user-level; Claude Code merges settings across user/project/local scopes and dedupes by command string, so one install covers every worktree without touching the user's repos):

```json
{
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "description": "Tethys session monitor",
        "hooks": [{ "type": "command", "command": "<app>/tethys-hook stop" }]
      }
    ],
    "Notification": [
      {
        "matcher": "",
        "description": "Tethys session monitor",
        "hooks": [{ "type": "command", "command": "<app>/tethys-hook notify" }]
      }
    ]
  }
}
```

_Install/uninstall:_ no CLI or SDK for this — it's read-modify-write on the JSON file. Create if missing, ensure the two arrays exist, append our entry if no existing entry has `description == "Tethys session monitor"`, then temp-file + `fsync` + atomic `rename`. Uninstall removes by description match. `matcher` is ignored for `Stop`/`Notification`.

_Companion binary (`tethys-hook`):_ ~30 lines. Reads JSON on stdin, connects to a Unix domain socket in the app data dir, writes one line, exits. If the socket isn't present (Tethys not running), exits 0 silently — the user's Claude session is never disrupted.

_What the hook receives_ (stdin JSON from Claude Code):

- Always: `session_id`, `cwd`, `hook_event_name`, `transcript_path`, `stop_hook_active`.
- `Stop` only: `last_assistant_message` — Claude's final reply text, no transcript parsing needed.
- `Notification` only: `message`, `notification_type` ∈ {`permission_prompt`, `idle_prompt`, `auth_success`, `elicitation_dialog`}. We surface these differently in the UI — a permission prompt is "blocking, decide now"; an idle prompt is just "Claude's ready when you are".

_Mapping hook events to our session:_ `session_id` on stdin maps directly to `claude_session.claude_session_id`. We capture that id when we spawn `claude` (either from its own first-message output or by diffing `~/.claude/projects/<hash>/` before/after spawn). No terminal parsing anywhere.

_Re-entrancy:_ Tethys only observes, never returns a blocking decision, so `stop_hook_active` is irrelevant for us. Documented here so we don't accidentally introduce blocking logic later.

**PrPoller.** For each attached PR, periodically runs `gh pr view <num> --json state,isDraft,reviewDecision,statusCheckRollup,headRefName`. Updates the `PrLink.status_json` field in `AppState`. Also polls on workspace focus. User attaches PRs manually in v1; auto-link-by-branch is a v2.

### "Attach" semantics

When the user clicks a session tab:

1. Frontend calls `attach_session(id)`; Rust returns recent scrollback.
2. Frontend writes scrollback into its xterm instance, then subscribes to `pty:data` for that session.
3. Resize events from xterm's fit addon → `resize` command.

This lets the UI mount/unmount xterm instances freely without losing output — Rust is always the source of truth.

### Directory layout (target)

```
tethys/
├── PLAN.md
├── README.md
├── src/                 # React frontend
├── src-tauri/           # Rust core + tauri config
│   ├── src/
│   │   ├── main.rs
│   │   ├── commands.rs  # #[tauri::command] glue
│   │   ├── store/       # AppState + state.json persistence
│   │   ├── sessions/    # PTY supervisor
│   │   ├── git/         # worktree ops
│   │   ├── hooks/       # HookBridge + tethys-hook companion
│   │   └── pr/          # gh poller
│   └── tauri.conf.json
└── crates/
    └── tethys-hook/     # companion CLI installed into claude settings
```

---

## Suggested build order (sanity check, not yet tactical)

1. Tauri skeleton + `AppState`/`state.json` + workspace CRUD (no Claude yet).
2. `repos.toml` loader + worktree create/delete + setup-script runner (block-on-failure, tear down on failure).
3. PTY supervisor + xterm.js wiring — can type into a shell in a worktree.
4. `claude` launch inside a worktree, plus `claude --resume` path for previously-known sessions.
5. HookBridge + turn detection (install hooks into `~/.claude/settings.json`, `tethys-hook` companion, turn flag in UI).
6. PR attach + `gh` poller.
7. Polish: pause, delete-with-force, VS Code open, setup-script failure UX.

Each of these ships a usable slice.
