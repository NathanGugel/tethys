use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::hook_listener::HookMessage;
use crate::state::SessionRuntimeState;
use crate::store::Store;

const RING_CAPACITY: usize = 2 * 1024 * 1024; // 2 MB scrollback per session
const READ_BUF: usize = 4096;

pub type SessionId = String;

/// Snapshot returned to the frontend for the sessions list. Does not include
/// the live byte stream — that flows over a `Channel` via `attach`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub workspace_id: String,
    pub repo_key: String,
    pub cwd: PathBuf,
    pub running: bool,
    pub runtime_state: SessionRuntimeState,
    /// Populated by the last Notification hook (e.g. `permission_prompt`).
    /// Set to `None` when state transitions away from `WaitingInput`.
    pub notification_type: Option<String>,
}

struct SessionHandle {
    info: SessionInfo,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    ring: Arc<Mutex<VecDeque<u8>>>,
    /// Fan-out targets for live PTY bytes. Writers that error (client closed)
    /// are dropped on the next tick.
    subscribers: Arc<Mutex<Vec<Channel<InvokeResponseBody>>>>,
    /// Flipped to `false` when the child process exits.
    running: Arc<Mutex<bool>>,
}

/// One entry per in-flight Claude spawn awaiting its `SessionStart` hook.
/// Cleaned up when the hook arrives or when the entry expires.
struct PendingSpawn {
    workspace_id: String,
    session_id: SessionId,
    expires_at: Instant,
}

const PENDING_TTL: Duration = Duration::from_secs(30);

/// Per-session ephemeral UI state (turn + last notification subtype).
/// Kept in-memory in the supervisor, not persisted — it's reconstructed
/// from scratch each run via new hook events.
#[derive(Debug, Default, Clone)]
struct TurnState {
    state: SessionRuntimeState,
    notification_type: Option<String>,
}

pub struct SessionSupervisor {
    sessions: Mutex<HashMap<SessionId, SessionHandle>>,
    /// Maps the `TETHYS_SPAWN_TOKEN` we set on the PTY env to the
    /// session metadata we need to update once Claude's SessionStart hook
    /// tells us the claude_session_id.
    pending: Mutex<HashMap<String, PendingSpawn>>,
    /// Per-session turn state. Keyed by Tethys `SessionId`.
    turn: Mutex<HashMap<SessionId, TurnState>>,
    store: Arc<Store>,
    app: AppHandle,
}

impl SessionSupervisor {
    pub fn new(app: AppHandle, store: Arc<Store>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            turn: Mutex::new(HashMap::new()),
            store,
            app,
        }
    }

    fn turn_of(&self, session_id: &str) -> TurnState {
        self.turn.lock().unwrap().get(session_id).cloned().unwrap_or_default()
    }

    /// Update a session's turn state + emit `session:turn_changed`.
    /// No-op if the new state matches the current one (other than forcing
    /// a notification_type refresh).
    fn set_turn(
        &self,
        session_id: &str,
        workspace_id: &str,
        state: SessionRuntimeState,
        notification_type: Option<String>,
    ) {
        let changed = {
            let mut map = self.turn.lock().unwrap();
            let current = map.entry(session_id.to_string()).or_default();
            if current.state == state && current.notification_type == notification_type {
                false
            } else {
                current.state = state;
                current.notification_type = notification_type.clone();
                true
            }
        };
        if !changed {
            return;
        }
        let _ = self.app.emit(
            "session:turn_changed",
            serde_json::json!({
                "workspace_id": workspace_id,
                "session_id": session_id,
                "runtime_state": state,
                "notification_type": notification_type,
            }),
        );
    }

    fn clear_turn(&self, session_id: &str, workspace_id: &str) {
        {
            let mut map = self.turn.lock().unwrap();
            map.remove(session_id);
        }
        let _ = self.app.emit(
            "session:turn_changed",
            serde_json::json!({
                "workspace_id": workspace_id,
                "session_id": session_id,
                "runtime_state": SessionRuntimeState::Dormant,
                "notification_type": Option::<String>::None,
            }),
        );
    }

    /// Optimistic: called from `send_input` so the UI flips to Working
    /// immediately without waiting for a hook to confirm Claude woke up.
    pub fn mark_working(&self, session_id: &str) {
        let workspace_id = {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(session_id).map(|h| h.info.workspace_id.clone())
        };
        if let Some(wsid) = workspace_id {
            self.set_turn(session_id, &wsid, SessionRuntimeState::Working, None);
        }
    }

    /// Low-level spawn: launches `program` with `args` in a fresh PTY.
    /// Extra env vars are applied on top of the inherited environment.
    pub fn spawn(
        &self,
        workspace_id: String,
        repo_key: String,
        cwd: &Path,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> AppResult<SessionInfo> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Other(format!("openpty failed: {e}")))?;

        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| AppError::Other(format!("spawn failed: {e}")))?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| AppError::Other(format!("clone reader failed: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AppError::Other(format!("take writer failed: {e}")))?;

        let id = new_session_id();
        let info = SessionInfo {
            id: id.clone(),
            workspace_id: workspace_id.clone(),
            repo_key,
            cwd: cwd.to_path_buf(),
            running: true,
            runtime_state: SessionRuntimeState::Working,
            notification_type: None,
        };

        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY)));
        let subscribers: Arc<Mutex<Vec<Channel<InvokeResponseBody>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(Mutex::new(true));

        spawn_reader_thread(reader, ring.clone(), subscribers.clone());
        spawn_child_watcher(
            child,
            id.clone(),
            workspace_id.clone(),
            running.clone(),
            self.app.clone(),
        );

        let handle = SessionHandle {
            info: info.clone(),
            master: pair.master,
            writer: Arc::new(Mutex::new(writer)),
            ring,
            subscribers,
            running,
        };

        self.sessions.lock().unwrap().insert(id.clone(), handle);
        // Default to Working — a just-spawned Claude is doing something
        // (starting up, replaying transcript). Hooks will refine shortly.
        self.turn.lock().unwrap().insert(
            id,
            TurnState {
                state: SessionRuntimeState::Working,
                notification_type: None,
            },
        );
        let _ = self.app.emit(
            "session:changed",
            serde_json::json!({ "workspace_id": workspace_id }),
        );
        Ok(info)
    }

    /// Spawn a `claude` process with a correlation token so the SessionStart
    /// hook can report back which Tethys session it belongs to. Pass
    /// `resume_claude_session_id` to resume an existing conversation.
    pub fn spawn_claude(
        &self,
        workspace_id: String,
        repo_key: String,
        cwd: &Path,
        claude_bin: &Path,
        resume_claude_session_id: Option<&str>,
    ) -> AppResult<(SessionInfo, String)> {
        let token = Uuid::new_v4().to_string();

        let mut args: Vec<String> = Vec::new();
        if let Some(id) = resume_claude_session_id {
            args.push("--resume".into());
            args.push(id.to_string());
        }

        let env = vec![("TETHYS_SPAWN_TOKEN".into(), token.clone())];
        let info = self.spawn(
            workspace_id.clone(),
            repo_key,
            cwd,
            &claude_bin.to_string_lossy(),
            &args,
            &env,
        )?;

        // Prune any expired pending correlations while we're here.
        let mut pending = self.pending.lock().unwrap();
        let now = Instant::now();
        pending.retain(|_, p| p.expires_at > now);
        pending.insert(
            token.clone(),
            PendingSpawn {
                workspace_id,
                session_id: info.id.clone(),
                expires_at: now + PENDING_TTL,
            },
        );

        Ok((info, token))
    }

    /// Dispatch a hook event from `tethys-hook`. Currently handles
    /// SessionStart (correlation), Stop (turn → Idle), and Notification
    /// (turn → WaitingInput).
    pub async fn handle_hook_event(&self, msg: HookMessage) {
        match msg.event.as_str() {
            "session-start" => self.handle_session_start(msg).await,
            "stop" => self.handle_stop(msg).await,
            "notify" => self.handle_notify(msg).await,
            other => debug!(event = %other, "unknown hook event"),
        }
    }

    async fn handle_stop(&self, msg: HookMessage) {
        let Some(csid) = msg.session_id.as_deref() else {
            return;
        };
        self.set_turn_by_claude_sid(csid, SessionRuntimeState::Idle, None)
            .await;
    }

    async fn handle_notify(&self, msg: HookMessage) {
        let Some(csid) = msg.session_id.as_deref() else {
            return;
        };
        // auth_success / elicitation_dialog don't represent a turn flip —
        // just log and bail. permission_prompt / idle_prompt both put the
        // session into WaitingInput; the notification_type is carried on
        // so the UI can render permission prompts more urgently.
        let state = match msg.notification_type.as_deref() {
            Some("permission_prompt") | Some("idle_prompt") => {
                SessionRuntimeState::WaitingInput
            }
            other => {
                debug!(
                    notification_type = ?other,
                    "ignoring Notification hook (non-turn event)"
                );
                return;
            }
        };
        self.set_turn_by_claude_sid(csid, state, msg.notification_type.clone())
            .await;
    }

    /// Find the Tethys session id that corresponds to a given Claude
    /// session_id and flip its turn state.
    async fn set_turn_by_claude_sid(
        &self,
        claude_session_id: &str,
        state: SessionRuntimeState,
        notification_type: Option<String>,
    ) {
        let lookup = self
            .store
            .read(|s| {
                for ws in &s.workspaces {
                    for sess in &ws.sessions {
                        if sess.claude_session_id.as_deref() == Some(claude_session_id) {
                            return Some((ws.id.clone(), sess.id.clone()));
                        }
                    }
                }
                None
            })
            .await;
        let Some((ws_id, sess_id)) = lookup else {
            debug!(
                claude_session_id,
                "hook for unknown Claude session (not a Tethys-spawned one)"
            );
            return;
        };
        self.set_turn(&sess_id, &ws_id, state, notification_type);
    }

    async fn handle_session_start(&self, msg: HookMessage) {
        let Some(token) = msg.spawn_token.as_deref() else {
            debug!("SessionStart without spawn_token — not a Tethys session");
            return;
        };
        let Some(claude_session_id) = msg.session_id.clone() else {
            warn!("SessionStart hook missing session_id");
            return;
        };

        let pending = {
            let mut pending = self.pending.lock().unwrap();
            pending.remove(token)
        };
        let Some(pending) = pending else {
            warn!(token, "SessionStart hook arrived with no matching pending spawn");
            return;
        };

        let transcript_path = msg.transcript_path.as_deref().map(PathBuf::from);
        let workspace_id = pending.workspace_id.clone();
        let session_id = pending.session_id.clone();

        let update = self
            .store
            .mutate(|state| {
                let Some(ws) = state.find_workspace_mut(&workspace_id) else {
                    return Ok(false);
                };
                let Some(session) = ws.sessions.iter_mut().find(|s| s.id == session_id)
                else {
                    return Ok(false);
                };
                session.claude_session_id = Some(claude_session_id.clone());
                session.transcript_path = transcript_path.clone();
                Ok(true)
            })
            .await;

        match update {
            Ok(true) => {
                info!(
                    %session_id,
                    %claude_session_id,
                    source = msg.source.as_deref().unwrap_or("?"),
                    "correlated SessionStart hook",
                );
                let _ = self.app.emit(
                    "workspace:changed",
                    serde_json::json!({ "workspace_id": workspace_id }),
                );
            }
            Ok(false) => warn!(
                %session_id,
                "SessionStart: no matching ClaudeSessionMeta in state"
            ),
            Err(e) => warn!(error = %e, "store mutate during SessionStart failed"),
        }
    }

    /// Register a new output subscriber and return the current scrollback.
    /// The frontend writes the scrollback into xterm first, then drains the
    /// channel for live bytes — zero gap.
    pub fn attach(
        &self,
        session_id: &str,
        channel: Channel<InvokeResponseBody>,
    ) -> AppResult<Vec<u8>> {
        let sessions = self.sessions.lock().unwrap();
        let handle = sessions
            .get(session_id)
            .ok_or_else(|| AppError::Other(format!("session not found: {session_id}")))?;

        let scrollback: Vec<u8> = handle.ring.lock().unwrap().iter().copied().collect();
        handle.subscribers.lock().unwrap().push(channel);
        Ok(scrollback)
    }

    pub fn send_input(&self, session_id: &str, data: &[u8]) -> AppResult<()> {
        let writer = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(session_id)
                .ok_or_else(|| AppError::Other(format!("session not found: {session_id}")))?
                .writer
                .clone()
        };
        writer
            .lock()
            .unwrap()
            .write_all(data)
            .map_err(|e| AppError::Other(format!("write: {e}")))?;
        Ok(())
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> AppResult<()> {
        let sessions = self.sessions.lock().unwrap();
        let handle = sessions
            .get(session_id)
            .ok_or_else(|| AppError::Other(format!("session not found: {session_id}")))?;
        handle
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Other(format!("resize: {e}")))?;
        Ok(())
    }

    pub fn list_for_workspace(&self, workspace_id: &str) -> Vec<SessionInfo> {
        let turn_map = self.turn.lock().unwrap().clone();
        let sessions = self.sessions.lock().unwrap();
        sessions
            .values()
            .filter(|h| h.info.workspace_id == workspace_id)
            .map(|h| {
                let mut info = h.info.clone();
                info.running = *h.running.lock().unwrap();
                let turn = turn_map.get(&h.info.id).cloned().unwrap_or_default();
                info.runtime_state = turn.state;
                info.notification_type = turn.notification_type;
                info
            })
            .collect()
    }
}

fn new_session_id() -> SessionId {
    Uuid::new_v4().to_string()
}

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    ring: Arc<Mutex<VecDeque<u8>>>,
    subscribers: Arc<Mutex<Vec<Channel<InvokeResponseBody>>>>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    debug!("pty reader: EOF");
                    break;
                }
                Ok(n) => {
                    let chunk = &buf[..n];
                    append_to_ring(&ring, chunk);
                    // Fan out, dropping subscribers whose channel errored.
                    let mut subs = subscribers.lock().unwrap();
                    subs.retain(|sub| {
                        sub.send(InvokeResponseBody::Raw(chunk.to_vec())).is_ok()
                    });
                }
                Err(e) => {
                    warn!(error = %e, "pty reader error");
                    break;
                }
            }
        }
    });
}

fn append_to_ring(ring: &Arc<Mutex<VecDeque<u8>>>, data: &[u8]) {
    let mut ring = ring.lock().unwrap();
    if data.len() >= RING_CAPACITY {
        ring.clear();
        ring.extend(&data[data.len() - RING_CAPACITY..]);
        return;
    }
    let overflow = (ring.len() + data.len()).saturating_sub(RING_CAPACITY);
    for _ in 0..overflow {
        ring.pop_front();
    }
    ring.extend(data.iter().copied());
}

fn spawn_child_watcher(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    session_id: SessionId,
    workspace_id: String,
    running: Arc<Mutex<bool>>,
    app: AppHandle,
) {
    std::thread::spawn(move || {
        let status = child.wait();
        *running.lock().unwrap() = false;
        let code = status.ok().map(|s| s.exit_code() as i32);
        info!(%session_id, ?code, "session child exited");
        let _ = app.emit(
            "session:exit",
            serde_json::json!({ "session_id": session_id, "code": code }),
        );
        // Turn state is stale once the PTY is gone — emit a Dormant
        // transition so the UI doesn't keep showing Working indefinitely.
        let _ = app.emit(
            "session:turn_changed",
            serde_json::json!({
                "workspace_id": workspace_id,
                "session_id": session_id,
                "runtime_state": SessionRuntimeState::Dormant,
                "notification_type": Option::<String>::None,
            }),
        );
    });
}
