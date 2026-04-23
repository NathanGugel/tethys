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

**Ships:** creating a workspace actually creates git worktrees and runs setup scripts. Deleting cleans up.

- [ ] `GitOps` module: `git clone`, `git worktree add/remove/list`, `git fetch` wrappers via `tokio::process::Command`.
- [ ] Per-repo clone-on-first-use: if `<data_dir>/repos/<repo_key>/.git` doesn't exist, `git clone <remote_url>` into it. Stream clone output to the same log pane as setup scripts (first clone of a big repo is slow — show progress).
- [ ] `create_workspace` (real): for each selected repo, ensure Tethys's clone exists, then `git -C <clone> worktree add <worktree_root>/<workspace_id>/<repo_key> -b <branch>`.
- [ ] Setup-script runner: async `Command` with piped stdout/stderr.
- [ ] Stream script output to the frontend (reuse the `tauri::ipc::Channel<Vec<u8>>` pattern we'll codify in M4 — plumb it now so the log pane component is generic over "any streamed process").
- [ ] Timeout: default 10 min, overridable per-repo in `repos.toml` (`setup_timeout_secs`).
- [ ] Cancel: UI button → SIGTERM, then SIGKILL after 5s grace.
- [ ] Failure path: if any script exits non-zero, tear down all worktrees created for this workspace and roll back `AppState`. Show the failing script's output in the modal.
- [ ] `delete_workspace`: stop sessions (stubbed for now — there are none yet), then `git worktree remove`. If dirty, refuse and surface the reason (force comes in M7).
- [ ] Modal log-pane component (reused later for PTY scrollback view and setup-script output).

**Verify:** create a workspace with a real repo whose setup script `exit 1`s → the partial worktree gets cleaned up and you see why.

---

## Milestone 4 — PTY + xterm.js

**Ships:** each session tab runs a real shell in its worktree. Tabs remember their output across mount/unmount.

- [ ] `SessionSupervisor` module owning a `HashMap<SessionId, SessionHandle>`.
- [ ] Spawn `portable-pty` with `cwd = worktree_path`. For M4, run the user's login shell (`$SHELL`), not `claude` — proves the pipeline without Claude in the loop.
- [ ] Per-session ring buffer (2 MB) holding recent PTY bytes.
- [ ] `attach_session({ session_id, channel })` command: return scrollback snapshot as the result; register `channel` as a subscriber; subsequent PTY reads fan out to all active subscribers.
- [ ] `send_input({ session_id, bytes })` writes to the PTY master.
- [ ] `resize({ session_id, cols, rows })` calls `master.resize()`.
- [ ] On PTY exit: emit `pty:exit`, drop subscribers, keep metadata in `AppState` until explicitly removed.
- [ ] Frontend: `SessionTerminal` React component mounts an xterm.js instance (canvas addon + fit + clipboard) on tab activation.
- [ ] On mount: create `Channel<Uint8Array>`, call `attach_session`, write returned scrollback, then drain the channel into xterm.
- [ ] On unmount: close channel. Verify PTY keeps running and ring buffer keeps filling.
- [ ] Fit addon wired to container resize → `resize` command.

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

- [ ] `Stop` hook handler: set session state to `Idle`, bump `last_turn_change_at`, emit `session:turn_changed`.
- [ ] `Notification` hook handler:
  - [ ] `permission_prompt` → state `WaitingInput` + `notification_type = permission_prompt` (shown distinctly in UI — "needs permission" vs plain "idle").
  - [ ] `idle_prompt` → state `WaitingInput`.
  - [ ] `auth_success`, `elicitation_dialog` → log for now; surface later if needed.
- [ ] Optimistic "working" state: on `send_input`, immediately flip state to `Working` so the UI is responsive without waiting for the next hook fire.
- [ ] UI indicators:
  - [ ] Session tab badge (color-coded dot).
  - [ ] Workspace row badge (rolled up from sessions).
  - [ ] App dock / menubar badge for total attention-needed count across all workspaces.
- [ ] `pause_workspace` / `resume_workspace` semantics (MVP):
  - [ ] Paused workspaces don't contribute to dock badge.
  - [ ] Paused workspaces don't trigger OS notifications.
  - [ ] Internal state still updates; unpause immediately reflects current truth.
- [ ] OS notification on transition to `WaitingInput` (unless paused). Click notification → focus the session tab.

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
