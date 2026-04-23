# Tethys — Tasks

Tactical breakdown of [PLAN.md](./PLAN.md), organized by milestone. Each milestone is a shippable slice — stop at any point and Tethys still does something useful.

Legend: `- [ ]` open · `- [x]` done.

---

## Milestone 1 — Skeleton & persistence

**Ships:** a Tauri app you can launch that lets you create/list/delete empty "workspaces" (no git yet). State survives restarts.

- [x] Scaffold Tauri 2.x app with React + TypeScript.
- [x] Add core Rust deps: `tokio`, `serde`, `serde_json`, `tracing`, `tracing-appender`, `uuid`, `chrono`, `anyhow`, `thiserror`. (`portable-pty` deferred to M4; `fs2` deferred to M5 hook lock — app-level single-instance handled by `tauri-plugin-single-instance` instead.)
- [x] Rolling log file: `tracing-subscriber` + `tracing-appender` daily-rolling to `<data_dir>/logs/tethys.log`.
- [x] Single-instance plugin (`tauri-plugin-single-instance`): second launch focuses the first window and exits.
- [x] `AppState` struct with `serde` derives matching the plan's data model (incl. `#[serde(skip)]` on ephemeral session fields + `#[serde(default)]` for forward-compat).
- [x] `Store`: `Arc<RwLock<AppState>>` + `store.mutate(|s| ...)` helper that nudges a debounced (~250ms) flusher.
- [x] Persistence writer: serialize → `state.json.tmp` → `fsync` → `rename`. Load on boot; create empty state if missing.
- [x] Tauri commands: `list_workspaces`, `get_workspace`, `create_workspace`, `delete_workspace`, `pause_workspace`, `resume_workspace`.
- [x] React UI skeleton: workspace list sidebar, create-workspace dialog (name + branch), detail pane, pause/delete affordances.
- [x] Wire `workspace:changed` event → frontend re-fetches via `list_workspaces`.

**Verify:** create a workspace, quit the app, relaunch → it's still there. Open two Tethys windows → second one focuses the first and exits.

---

## Milestone 2 — Repo registry

**Ships:** workspaces bind to repos declared in `repos.toml`. Create dialog lets you multi-select repos. Empty-state guides the user when the registry is missing/invalid.

- [x] `RepoRegistry` struct + TOML deserialization. Required `worktree_root` field; `[[repo]]` array with `key`, `display_name`, `origin_path`, `default_setup_script`, optional `setup_timeout_secs`.
- [x] Load `repos.toml` on boot from `<data_dir>/repos.toml`. Missing or invalid → surfaced to the UI; app boots normally but "New workspace" is disabled.
- [x] Validate `worktree_root`: `create_dir_all`, then a write-probe to confirm it's writable. Any failure surfaces as an `Invalid` registry status.
- [x] `list_repos` and `registry_status` commands expose registry state to the frontend.
- [x] `open_repos_config` command: writes a starter template if `repos.toml` doesn't exist, then shells out to `open` (macOS). User restarts to pick up edits (file-watcher deferred).
- [x] Update create-workspace dialog: multi-select checkboxes per registered repo; require ≥1 selected.
- [x] `create_workspace` validates each selected key against the registry and populates `repo_links` with *planned* worktree paths (`<worktree_root>/<workspace_id>/<repo_key>`). Actual `git worktree add` still deferred to M3.

**Verify:** launch with no `repos.toml` → empty-state card with "Open repos.toml" button. After filling in the template and restarting → create dialog shows repos as checkboxes; creating a workspace populates its detail pane with the planned worktree paths (marked "not created yet").

**Deferred to M3** (these needed worktrees-on-disk to reconcile against, and needed `GitOps` to implement Repair):

- Boot-time worktree reconciler (orphaned dirs, missing worktrees).
- Repair / Forget actions.

---

## Milestone 3 — Worktree & setup-script lifecycle

**Ships:** creating a workspace actually creates git worktrees and runs setup scripts. Deleting cleans up. Crash-recovery via a reconciler that diffs state.json against disk.

- [x] `GitOps` module: `git clone`, `git worktree add/remove` wrappers via `tokio::process::Command`. (`fetch` / `list` not yet needed.)
- [x] Per-repo clone-on-first-use into `<data_dir>/repos/<repo_key>/`. `git clone --progress` + self-heal (partial clones detected via `git rev-parse HEAD` and wiped).
- [x] `create_workspace` (real): clone → worktree add → optional setup script per repo, all streamed.
- [x] Setup-script runner: async `/bin/sh -c <script>`, piped stdio, line-by-line streaming.
- [x] Stream output via `tauri::ipc::Channel<JobEvent>` — backend-to-frontend over a single typed channel per job. Line reader splits on both `\n` and `\r` so git/yarn progress updates surface.
- [x] Timeout: default 10 min, per-repo override via `setup_timeout_secs` in `repos.toml`. On timeout: SIGTERM → 5s grace → SIGKILL.
- [ ] Cancel: UI button → SIGTERM, then SIGKILL after 5s grace. (Deferred — would require a cancel signal threaded through the job.)
- [x] Failure path: on any per-repo error, tear down every worktree already created for the workspace; workspace is never added to state (atomic success).
- [x] `delete_workspace`: runs `git worktree remove` for each repo_link, then clears state. (Non-force only; dirty-force → M7.)
- [x] Modal log-pane component (`JobLogModal`, reused for both create and delete jobs).
- [x] Boot-time reconciler: `list_discrepancies` command diffs state.json against disk. UI surfaces orphaned worktrees with a Remove button and workspaces-with-missing-worktrees with a Forget button.

**Verify:** create a workspace with a real repo whose setup script `exit 1`s → the partial worktree gets cleaned up and you see why.

---

## Milestone 4 — PTY + xterm.js

**Ships:** each session tab runs a real shell in its worktree. Tabs remember their output across mount/unmount.

- [x] `SessionSupervisor` module with `HashMap<SessionId, SessionHandle>` guarded by `Mutex`.
- [x] Spawn `portable-pty` with `cwd = worktree_path`. M4 runs `$SHELL`; M5 swaps the program for `claude`.
- [x] Per-session ring buffer (2 MB) holding recent PTY bytes.
- [x] `attach_session({ session_id, on_bytes })` command: returns scrollback as `Vec<u8>` (the command result), registers the `Channel<InvokeResponseBody>` for live fan-out.
- [x] `send_input({ session_id, data })` writes to the PTY master.
- [x] `resize_session({ session_id, cols, rows })` calls `master.resize()`.
- [x] Child watcher thread emits `session:exit` and flips `running=false`; handle stays in the map so scrollback survives for re-attach.
- [x] `SessionTerminal` component mounts an xterm.js instance (canvas addon + fit + clipboard) and uses raw-bytes `Channel<ArrayBuffer>` for zero JSON-overhead streaming.
- [x] On mount: scrollback first, then live stream. On unmount: dispose xterm; backend drops the subscriber on its next send.
- [x] Fit addon wired to a `ResizeObserver` on the container → `resize_session`.

**Verify:** open a tab, `yes | head -10000`, switch tabs, switch back → no gap, scrollback intact. Reload the webview → still intact.

---

## Milestone 5 — Claude launch & session-id capture

**Ships:** workspace sessions run real `claude`. Tethys knows each session's `claude_session_id` and can `--resume` after restart.

- [ ] `claude` path resolution: run `/bin/zsh -ilc 'which claude'` once at boot, cache absolute path. Re-resolve lazily if the cached path stops working.
- [ ] `crates/tethys-hook/` companion binary with subcommands `session-start`, `stop`, `notify`.
  - [ ] Reads stdin JSON, reads `TETHYS_SPAWN_TOKEN` from its env (for `session-start`), writes a length-prefixed JSON frame to `<data_dir>/hook.sock`, exits 0.
  - [ ] If the socket is unreachable, exits 0 silently — never disrupts the user's Claude session.
- [ ] UDS listener in Rust core: `tokio::net::UnixListener` on `<data_dir>/hook.sock`. Parses frames, dispatches by event type.
- [ ] Hook installer:
  - [ ] Acquire `flock` on `<data_dir>/claude-settings.lock`.
  - [ ] Read `~/.claude/settings.json` (create if missing).
  - [ ] Ensure `hooks.SessionStart`, `hooks.Stop`, `hooks.Notification` arrays exist.
  - [ ] If no entry has `description == "Tethys session monitor"`, append ours.
  - [ ] Atomic write (`.tmp` + `fsync` + `rename`), then re-read and verify our entry survived.
  - [ ] Run install on every boot (idempotent by design).
- [ ] Update `SessionSupervisor.start_claude_session`:
  - [ ] Generate `TETHYS_SPAWN_TOKEN = <uuid>`.
  - [ ] Spawn `claude` via `portable-pty` with `TETHYS_SPAWN_TOKEN` in the env.
  - [ ] Register a pending correlation: `{ token → session_row_id }`.
  - [ ] Wait for matching `SessionStart` event (10s timeout). On arrival: write `claude_session_id` + `transcript_path` into `ClaudeSessionMeta`.
  - [ ] On timeout: mark session errored, show diagnostic ("Claude started but we never got a SessionStart hook — is `~/.claude/settings.json` writable?").
- [ ] `resume_claude_session`: spawn `claude --resume <claude_session_id>` in the same worktree; `SessionStart` fires with `source: "resume"` and the correlation flow works identically.
- [ ] On app quit: send EOT to PTYs, then kill after short grace.

**Verify:** start a Claude session, have a short conversation, quit the app, relaunch, click "resume" on the dormant session — you pick up where you left off.

---

## Milestone 6 — Turn detection & pause

**Ships:** the core feature — workspaces flag themselves when it's your turn. Pause silences the flag until you revive.

- [x] `Stop` hook handler: set session state to `Idle`, emit `session:turn_changed`.
- [x] `Notification` hook handler: `permission_prompt` / `idle_prompt` → `WaitingInput` (carrying `notification_type` so UI can mark permission prompts urgent). `auth_success` / `elicitation_dialog` logged and ignored.
- [x] Optimistic "working" on `send_input` so the UI reacts on keystroke, not on next hook.
- [x] Session chip dot color-coded by runtime state; repo tab aggregates to the most-urgent state across its sessions.
- [x] Workspace row badge in sidebar (amber dot) when any session is `WaitingInput` and the workspace isn't paused.
- [x] Pause semantics: paused workspaces suppress the sidebar attention dot; internal state keeps updating so unpause shows current truth.
- [ ] App dock / menubar badge with total attention count. (Deferred.)
- [ ] OS notification on transition to `WaitingInput`. (Deferred — needs Tauri notification plugin wiring.)

**Verify:** start Claude in two workspaces, ask one a question, wait for it to ask back — that workspace lights up, the other stays calm.

---

## Milestone 7 — Polish

**Ships:** everything that makes it pleasant to actually use.

- [ ] `open_vscode({ workspace_id, repo_key })`: shell out to `code <worktree_path>`. If `code` is missing from PATH, surface "install `code` CLI" help.
- [ ] Force-delete: when `delete_workspace` fails due to dirty worktree, UI shows "Worktree has uncommitted changes" with "Force delete" confirm → `git worktree remove --force`.
- [ ] Setup-script failure recovery: "Retry" button on the failure modal re-runs just the failing script against the already-created worktree (skip workspace teardown if the user opts in).
- [ ] Empty-state UIs: no `repos.toml` → guided setup. No workspaces → "Create your first workspace" CTA.
- [ ] Hook uninstall on app uninstall: document the manual cleanup (or add a menu item — cheap).
- [ ] Keyboard shortcuts: Cmd-N new workspace, Cmd-W close tab, Cmd-` cycle sessions (stretch).
- [ ] Update `tethys-hook` path in `~/.claude/settings.json` if the app is moved (resolve against current binary location on every install).

---

## Deferred (v1.1+)

Not tasked — captured here so we don't forget the conversations.

- PR status / `gh` polling + `gh auth status` probe on first use.
- `repos.toml` file-watcher so edits apply without restart.
- Per-session "Deleting" state machine for delete-while-responding safety.
- Reveal-in-Finder workspace action.
- Auto-link PRs by branch name.
- `__tethys_managed: true` marker for hook entries (more robust than description-match).
- Detach-on-quit / re-attach-on-launch for truly background sessions.
