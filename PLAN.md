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
- Delete a workspace — cleans up worktrees and session state.

**Out of scope for v1:**

- PR status / `gh` polling (deferred to v1.1).
- Multi-machine sync / cloud state.
- Creating PRs from inside Tethys.
- Merge / rebase / conflict UI.
- Editing `.claude/settings.json` beyond Tethys-managed hooks.
- Per-workspace env var management (inherits shell env for MVP).
- Multi-user support.

---

## Stack

- **Tauri 2.x** shell.
- **Rust core** (`src-tauri/`): state store, PTY supervision, git shell-outs, hook event ingestion, filesystem watching, rolling logs.
- **TypeScript + React** frontend (`src/`): workspace list, session tabs, xterm.js hosts.
- **xterm.js** with the **canvas addon** (not WebGL — WebGL + WKWebView has GPU context-loss and retina-blur issues on macOS, and the WebGL addon is in maintenance mode); `fit` and `clipboard` addons.
- **`portable-pty`** (Rust) for PTY spawning; one PTY per Claude session.
- **`tauri::ipc::Channel<T>`** for streaming PTY bytes from Rust to the frontend. The Tauri event bus is JSON-serialized and has documented throughput issues under sustained load ([#8177](https://github.com/tauri-apps/tauri/issues/8177), [#3021](https://github.com/tauri-apps/tauri/issues/3021)); channels are the official recommendation for high-rate child-process stdout. Events are reserved for low-rate signals (turn changes, session exit, workspace mutations).
- **In-memory state + JSON file** for persistence. One `state.json` in the app data dir; in-memory `AppState` is the source of truth, flushed atomically (temp file + `fsync` + `rename`) on a debounced writer.
- **Shell-outs** to `git` for MVP (rather than libgit2). Simpler, matches what the user types.
- **Rolling log file** in the app data dir from day one — PTY supervisor, hook socket, and git shellouts are otherwise impossible to debug.

---

## Architecture

### Process model

One Tauri app, two halves connected by Tauri commands + events:

```
┌─────────────────────────────┐        ┌──────────────────────────────┐
│  Frontend (WebView)         │        │  Rust core                   │
│  - React UI                 │ cmds → │  - SessionSupervisor (PTYs)  │
│  - xterm.js per session     │ ← evts │  - AppState (mem + state.json)│
│  - turn indicators          │ ← chan │  - GitOps (worktree CLI)     │
│                             │        │  - HookBridge (UDS listener) │
└─────────────────────────────┘        └──────────────────────────────┘
```

PTY byte streams flow over per-session `tauri::ipc::Channel`s (shown as `chan` above); commands + low-rate events flow over the normal IPC bus.

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
- `attach_session({ session_id, channel })` → returns recent scrollback; subsequent PTY bytes flow over the provided `tauri::ipc::Channel`
- `open_vscode({ workspace_id, repo_key })`

**Events (Rust → JS):** (low-rate only — PTY data goes over channels)

- `pty:exit` `{ session_id, code }`
- `session:turn_changed` `{ session_id, state: "idle" | "waiting_input" | "working", notification_type? }`
- `workspace:changed` `{ workspace_id }`

### Data model

Two separate pieces of state:

1. **`RepoRegistry`** — user-edited TOML at `<data_dir>/repos.toml`. Loaded on boot, never written by Tethys.
2. **`AppState`** — Tethys-managed, persisted to `<data_dir>/state.json`.

#### Repo registry (`repos.toml`, user-edited)

```toml
# Where Tethys creates worktrees. User-controlled so they can live on a
# preferred volume / outside macOS "Application Support" path quirks.
worktree_root = "/Users/ryan/code/tethys-worktrees"

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

`worktree_root` is required — Tethys refuses to start a workspace create without it. Deserialized into a read-only `RepoRegistry` held alongside `AppState`. For MVP, edits require an app restart; a file-watcher can come later.

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
}

struct RepoLink { repo_key, worktree_path, setup_script_ran_at }

struct ClaudeSessionMeta {
    id, repo_key, cwd, claude_session_id, transcript_path,
    // ephemeral, not persisted:
    #[serde(skip)] pid, state, last_turn_change_at,
}
```

`branch` lives on `Workspace` (not `RepoLink`) because a workspace uses a single branch name across every repo it spans — chosen by the user at creation time.

`transcript_path` is persisted alongside `claude_session_id` as a fallback: if `claude --resume` ever changes in a breaking way, we can still show the transcript read-only so the user doesn't lose visibility into dormant sessions.

**What's persisted vs ephemeral:**

- Persisted: workspaces, repo_links, session metadata (id, cwd, claude_session_id, transcript_path).
- Ephemeral (in-memory only, not written): session `pid` / `state` / `last_turn_change_at`, PTY ring buffers.

Schema evolution uses `#[serde(default)]` on new fields — no migrations for MVP.

### Key subsystems

**AppState / Store.** `RwLock<AppState>` in a shared `Arc`. All mutations go through a `store.mutate(|s| { ... })` helper that (a) applies the change, (b) emits a `workspace:changed` event, (c) schedules a debounced flush (~250ms) to `state.json`. Writer serializes to `state.json.tmp`, `fsync`s, `rename`s. Losing the last <1s of writes on a hard crash is acceptable; nothing we persist is unrecoverable.

*Single-instance lock.* On launch, Tethys acquires an advisory `flock` on `<data_dir>/tethys.lock`. If it fails, a second instance is already running — the new process signals the existing one to focus its window and exits. This prevents two processes racing on `state.json` (our persistence model has no inter-process concurrency story).

**GitOps.** Shells out to `git worktree add/remove/list`. On workspace create: one worktree per selected repo, at `<worktree_root>/<workspace>/<repo>` (`worktree_root` from `repos.toml`, picked by the user — avoids the macOS `Application Support` path-with-spaces hazard for setup scripts that don't quote `$PWD` correctly). Runs the repo's setup script after worktree creation. On delete: stops sessions, then `git worktree remove`; prompts for `--force` if dirty.

*Branch naming:* workspace creation takes a single `branch` name (user-supplied in the create dialog) that's used for every repo in the workspace: `git worktree add <path> -b <branch>`. No templating, no per-repo overrides in v1.

*Setup script failures:* if any repo's setup script exits non-zero, workspace creation is **blocked** — we tear down the worktrees we already created for the workspace and surface the script's stdout/stderr to the UI. The user fixes the problem and retries from a clean slate. No partially-created workspaces.

*Setup script UX:* scripts run async with stdout/stderr streamed live into a modal log pane (same log-view component we'll use for scrollback). Each script has a hard timeout (default 10 min; configurable per-repo in `repos.toml`). A cancel button sends SIGTERM, then SIGKILL after a 5-second grace.

*Boot-time reconciler:* on app start, Tethys lists `<worktree_root>/` and cross-checks against `AppState.workspaces`. Worktrees on disk with no corresponding workspace (crashed mid-create) surface as "orphaned — remove?" in the UI. Workspaces in state with missing worktrees surface as "worktree missing — repair or forget?".

**SessionSupervisor.** Owns `portable-pty` children. Each Claude session is one PTY running `claude` (or `claude --resume <id>`) with `cwd=worktree_path`. Reads stdout into a ring buffer (for late-attaching UIs) and **streams bytes to the frontend via the per-session `tauri::ipc::Channel` established by `attach_session`** — never via `emit`. Writes to stdin on `send_input`. On process exit, emits `pty:exit` and updates `AppState`.

*`claude` resolution.* Desktop apps on macOS inherit a minimal `$PATH` (no `/opt/homebrew/bin`, no nvm shims). On first boot, Tethys runs `/bin/zsh -ilc 'which claude'` once to resolve the absolute path, caches it in memory, and re-resolves if the binary disappears. Users who installed `claude` via nvm/volta/homebrew "just work" without needing to edit a plist.

*Session-id capture via `SessionStart` hook + correlation token.* Tethys generates a short UUID correlation token per launch and sets it as `TETHYS_SPAWN_TOKEN=<uuid>` in the PTY's environment. A `SessionStart` hook (installed by HookBridge alongside `Stop`/`Notification`) runs `tethys-hook session-start`, which reads `TETHYS_SPAWN_TOKEN` from its own env and relays it alongside Claude's `session_id` + `source` + `transcript_path` over the UDS. The Rust core correlates the token back to the `ClaudeSessionMeta` row we just created and writes in the `claude_session_id` + `transcript_path`. No stdout scraping, no project-dir diffing, no race.

*Across app restarts:* on app quit, Tethys kills its PTYs — the `claude` processes terminate with them. `claude_session_id` is persisted in `state.json`, so on next launch the UI shows each previous session as dormant-but-resumable; clicking it spawns a new PTY running `claude --resume <id>` in the same worktree. We lean entirely on Claude Code's own session resumption — no daemon, no detached PTYs, no state we have to reconstruct ourselves.

**HookBridge — the "your turn" mechanism.**

Tethys installs three hooks into `~/.claude/settings.json` on first run (user-level; Claude Code merges settings across user/project/local scopes and dedupes by command string, so one install covers every worktree without touching the user's repos):

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "",
        "description": "Tethys session monitor",
        "hooks": [{ "type": "command", "command": "<app>/tethys-hook session-start" }]
      }
    ],
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

_Install/uninstall:_ no CLI or SDK for this — it's read-modify-write on the JSON file. Wrapped in an advisory `flock` on `<data_dir>/claude-settings.lock` to serialize against other Tethys processes; re-read after write to verify our entry survived (guards against a user editing the file in a text editor mid-install). Create if missing, ensure the three arrays exist, append our entry if no existing entry has `description == "Tethys session monitor"`, then temp-file + `fsync` + atomic `rename`. Uninstall removes by description match. `matcher` is ignored for these events.

_Companion binary (`tethys-hook`):_ ~30 lines. Reads JSON on stdin, connects to a Unix domain socket in the app data dir, writes one line, exits. If the socket isn't present (Tethys not running), exits 0 silently — the user's Claude session is never disrupted.

_What the hook receives_ (stdin JSON from Claude Code):

- Always: `session_id`, `cwd`, `hook_event_name`, `transcript_path`, `stop_hook_active`.
- `SessionStart` only: `source` ∈ {`startup`, `resume`, `clear`, `compact`} — tells us whether this is a fresh launch or a `--resume`.
- `Stop` only: `last_assistant_message` — Claude's final reply text, no transcript parsing needed.
- `Notification` only: `message`, `notification_type` ∈ {`permission_prompt`, `idle_prompt`, `auth_success`, `elicitation_dialog`}. We surface these differently in the UI — a permission prompt is "blocking, decide now"; an idle prompt is just "Claude's ready when you are".

_Mapping hook events to our session:_ on `SessionStart`, `tethys-hook` reads `TETHYS_SPAWN_TOKEN` from its own env and sends `{ token, session_id, transcript_path, source }` over the UDS. The Rust core looks up the pending `ClaudeSessionMeta` by token and writes in `claude_session_id` + `transcript_path`. Subsequent `Stop` / `Notification` events carry `session_id` directly, which now maps cleanly to our row. No terminal parsing, no filesystem diffing.

_Re-entrancy:_ Tethys only observes, never returns a blocking decision, so `stop_hook_active` is irrelevant for us. Documented here so we don't accidentally introduce blocking logic later.

### "Attach" semantics

When the user clicks a session tab:

1. Frontend creates a `tauri::ipc::Channel<Vec<u8>>` and calls `attach_session({ session_id, channel })`.
2. Rust returns recent scrollback (from the ring buffer) synchronously as the command result; from that moment on, all new PTY bytes are written to the channel.
3. Frontend writes scrollback into its xterm instance first, then begins draining the channel into the same xterm — this gives a seamless catch-up from historical bytes into live bytes with no gap.
4. Resize events from xterm's fit addon → `resize` command.
5. On unmount, frontend closes the channel; Rust detects the close and drops that subscriber (the PTY keeps running, the ring buffer keeps filling).

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
│   │   └── hooks/       # HookBridge + tethys-hook companion
│   └── tauri.conf.json
└── crates/
    └── tethys-hook/     # companion CLI installed into claude settings
```

---

## Suggested build order (sanity check, not yet tactical)

1. Tauri skeleton + rolling log + single-instance `flock` + `AppState`/`state.json` + workspace CRUD (no Claude yet).
2. `repos.toml` loader (with `worktree_root`) + workspace CRUD end-to-end + boot-time worktree reconciler.
3. Worktree create/delete + setup-script runner (async, streamed output, timeout, cancel, tear-down on failure).
4. PTY supervisor **using `tauri::ipc::Channel`** + xterm.js (canvas addon) wiring — can type into a shell in a worktree.
5. `claude` launch with `$PATH` resolution via login shell + `SessionStart` hook + `TETHYS_SPAWN_TOKEN` correlation + `tethys-hook` companion + UDS listener. End of this step: Tethys launches Claude and knows its `session_id`.
6. `Stop` + `Notification` hooks + turn UI + pause/resume.
7. Polish: VS Code open, setup-script failure UX, delete-with-force.

Each of these ships a usable slice. PR polling is explicitly deferred to v1.1.
