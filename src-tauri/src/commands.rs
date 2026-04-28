use std::sync::Arc;

use chrono::Utc;
use serde::Deserialize;
use tauri::{ipc::Channel, AppHandle, Emitter, State};
use tracing::{info, warn};

use std::path::PathBuf;

use tauri::ipc::InvokeResponseBody;

use crate::claude;
use crate::claude_local;
use crate::error::{AppError, AppResult};
use crate::git;
use crate::github::poller::{AuthSnapshot, GithubPoller};
use crate::inprogress::InProgressWorkspaces;
use crate::job::{JobEvent, JobTx};
use crate::paths::Paths;
use crate::purge::Purger;
use crate::reconcile::{self, Discrepancies};
use crate::registry::{self, starter_template, RegistryLoad, Repo};
use crate::sessions::{SessionInfo, SessionSupervisor};
use crate::setup;
use crate::state::{
    new_workspace_id, ClaudeSessionMeta, RepoLink, SystemErrorEntry, Workspace, WorkspaceId,
};
use crate::store::Store;
use crate::theme::Theme;
use crate::tmux::{self, TmuxBin};

#[tauri::command]
pub async fn list_workspaces(store: State<'_, Arc<Store>>) -> AppResult<Vec<Workspace>> {
    Ok(store.read(|s| s.workspaces.clone()).await)
}

#[tauri::command]
pub async fn get_workspace(
    store: State<'_, Arc<Store>>,
    id: WorkspaceId,
) -> AppResult<Workspace> {
    store
        .read(|s| s.find_workspace(&id).cloned())
        .await
        .ok_or_else(|| AppError::WorkspaceNotFound(id.clone()))
}

#[tauri::command]
pub fn list_repos(registry: State<'_, Arc<RegistryLoad>>) -> AppResult<Vec<Repo>> {
    let reg = registry.require()?;
    Ok(reg.repos.clone())
}

#[tauri::command]
pub fn registry_status(registry: State<'_, Arc<RegistryLoad>>) -> RegistryLoad {
    (**registry).clone()
}

#[tauri::command]
pub async fn github_auth_status(
    poller: State<'_, Arc<GithubPoller>>,
) -> AppResult<AuthSnapshot> {
    Ok(poller.auth_snapshot().await)
}

#[tauri::command]
pub async fn github_reprobe_auth(
    poller: State<'_, Arc<GithubPoller>>,
) -> AppResult<AuthSnapshot> {
    poller.probe_login().await;
    Ok(poller.auth_snapshot().await)
}

#[tauri::command]
pub fn open_repos_config(paths: State<'_, Paths>) -> AppResult<()> {
    let path = paths.repos_config_file();
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, starter_template())?;
        info!(?path, "wrote starter repos.toml");
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&path)
            .status()
            .map_err(|e| AppError::Other(format!("failed to open {}: {e}", path.display())))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        return Err(AppError::Other(
            "open_repos_config is only implemented for macOS in M2".into(),
        ));
    }

    Ok(())
}

#[tauri::command]
pub async fn open_in_vscode(
    store: State<'_, Arc<Store>>,
    id: WorkspaceId,
) -> AppResult<()> {
    let paths: Vec<PathBuf> = store
        .read(|s| {
            s.find_workspace(&id)
                .map(|w| w.repo_links.iter().map(|r| r.worktree_path.clone()).collect())
        })
        .await
        .ok_or_else(|| AppError::WorkspaceNotFound(id.clone()))?;

    if paths.is_empty() {
        return Err(AppError::Other(format!(
            "workspace {id} has no repos to open"
        )));
    }

    for path in &paths {
        std::process::Command::new("open")
            .args(["-a", "Visual Studio Code"])
            .arg(path)
            .status()
            .map_err(|e| {
                AppError::Other(format!("failed to open {} in VS Code: {e}", path.display()))
            })?;
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceArgs {
    pub branch: String,
    pub repo_selections: Vec<String>,
    /// Optional alternate entry-point binary name (e.g. `claude-hipaa`).
    /// Resolved on the login-shell PATH at spawn time.
    #[serde(default)]
    pub claude_binary: Option<String>,
}

/// Orchestrates clone + worktree add + setup script for every selected repo,
/// streaming progress to the frontend via `on_event`. On any failure, tears
/// down every worktree it already created before returning the error.
///
/// The workspace only lands in `AppState` on full success — so a crash mid-way
/// leaves worktrees on disk but no `state.json` entry (handled by the
/// boot-time reconciler).
#[tauri::command]
pub async fn create_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    registry: State<'_, Arc<RegistryLoad>>,
    paths: State<'_, Paths>,
    in_progress: State<'_, InProgressWorkspaces>,
    args: CreateWorkspaceArgs,
    on_event: Channel<JobEvent>,
) -> AppResult<Workspace> {
    let branch = args.branch.trim().to_string();
    if branch.is_empty() {
        return Err(AppError::Other("branch is required".into()));
    }
    if args.repo_selections.is_empty() {
        return Err(AppError::Other(
            "pick at least one repo to include in the workspace".into(),
        ));
    }
    let claude_binary = args
        .claude_binary
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Validate up-front so the user finds out before we clone repos.
    if let Some(bin) = claude_binary.as_deref() {
        claude::resolve_named(bin)?;
    }

    let reg = registry.require()?;
    let selected: Vec<Repo> = args
        .repo_selections
        .iter()
        .map(|k| {
            reg.find_repo(k)
                .cloned()
                .ok_or_else(|| AppError::Other(format!("unknown repo key: {k}")))
        })
        .collect::<AppResult<Vec<_>>>()?;

    let id = new_workspace_id();
    let workspace_dir = registry::sanitize_branch_for_dir(&branch);
    // Block collisions before we start cloning/fetching. Two workspaces with
    // the same branch on different repo sets would otherwise share a parent
    // dir, and deleting one would clobber the other on the `rm -rf` step.
    let workspace_root = reg.worktree_root.join(&workspace_dir);
    if workspace_root.exists() {
        return Err(AppError::Other(format!(
            "a worktree directory already exists at {}. Pick a different \
             branch name, or remove the existing directory first.",
            workspace_root.display()
        )));
    }
    // Register as in-progress so the reconciler doesn't flag our worktree
    // dirs as orphans mid-create. Guard removes the entry on drop — normal
    // return, `?`, panic, or task cancellation.
    let _in_progress_guard = in_progress.insert(workspace_dir.clone());
    let tx = spawn_event_forwarder(on_event);

    let orchestrate = async {
        let mut created: Vec<RepoLink> = Vec::new();
        for repo in &selected {
            let clone_path = paths.repo_clone_path(&repo.key);
            let worktree_path = reg.plan_worktree_path(&workspace_dir, &repo.key);

            git::ensure_clone(&clone_path, &repo.remote_url, &tx, &repo.key).await?;
            git::pull_clone_best_effort(&clone_path, &tx, &repo.key).await;

            // Pre-check: if the branch already exists, git worktree add will
            // fail with a fatal. We bail here with a clearer message — and
            // avoid partially-creating worktrees in other repos first.
            if git::branch_exists(&clone_path, &branch).await? {
                return Err(AppError::Other(format!(
                    "branch '{branch}' already exists in {}. Pick a different \
                     branch name, or delete the stale branch first.",
                    repo.key
                )));
            }

            git::worktree_add(&clone_path, &worktree_path, &branch, &tx, &repo.key).await?;

            claude_local::install_symlink(
                &worktree_path,
                &paths.repo_shared_claude_local(&repo.key),
                &tx,
                &repo.key,
            )
            .await?;

            let mut link = RepoLink {
                repo_key: repo.key.clone(),
                worktree_path: worktree_path.clone(),
                setup_script_ran_at: None,
                github: None,
            };

            if let Some(script) = repo.default_setup_script.as_ref().filter(|s| !s.trim().is_empty()) {
                setup::run_setup_script(
                    script,
                    &worktree_path,
                    repo.setup_timeout_secs,
                    &tx,
                    &repo.key,
                )
                .await?;
                link.setup_script_ran_at = Some(Utc::now());
            }

            created.push(link);
        }
        Ok::<_, AppError>(created)
    };

    match orchestrate.await {
        Ok(created_links) => {
            let workspace = Workspace {
                id: id.clone(),
                branch,
                paused: false,
                created_at: Utc::now(),
                repo_links: created_links,
                sessions: Vec::new(),
                claude_binary,
                deleted_at: None,
                archived_at: None,
            };

            let stored = store
                .mutate(|s| {
                    s.workspaces.push(workspace.clone());
                    Ok(workspace.clone())
                })
                .await?;

            info!(id = %stored.id, branch = %stored.branch, repos = stored.repo_links.len(), "created workspace");
            let _ = tx.0.send(JobEvent::Success);
            emit_workspace_changed(&app, &stored.id);
            Ok(stored)
        }
        Err(e) => {
            let msg = e.to_string();
            warn!(error = %msg, "workspace create failed; rolling back worktrees");
            tx.status(format!("tearing down partial workspace: {msg}"), None);

            // Best-effort teardown of anything we managed to create.
            // For each repo where the worktree dir exists, we know both the
            // worktree and the branch are ours to remove (the pre-check
            // above guarantees we didn't inherit a pre-existing branch).
            for repo in selected.iter().rev() {
                let clone_path = paths.repo_clone_path(&repo.key);
                let worktree_path = reg.plan_worktree_path(&workspace_dir, &repo.key);
                if !worktree_path.exists() {
                    continue;
                }
                if let Err(cleanup_err) = git::worktree_remove(
                    &clone_path,
                    &worktree_path,
                    true, // force: we created it, we're tearing it down
                    &tx,
                    &repo.key,
                )
                .await
                {
                    tx.status(
                        format!("cleanup failed for {}: {cleanup_err}", repo.key),
                        Some(&repo.key),
                    );
                }
                git::worktree_prune_best_effort(&clone_path, &tx, &repo.key).await;
                git::branch_delete_best_effort(&clone_path, &branch, &tx, &repo.key)
                    .await;
            }

            // Remove the now-empty parent dir so the reconciler doesn't
            // flag it as an orphan on the next tick.
            let parent = reg.worktree_root.join(&workspace_dir);
            if parent.exists() && reconcile::is_under(&reg.worktree_root, &parent) {
                if let Err(e) = tokio::fs::remove_dir_all(&parent).await {
                    warn!(path = %parent.display(), error = %e, "failed to remove partial workspace dir");
                }
            }

            let _ = tx.0.send(JobEvent::Failed { error: msg });
            Err(e)
        }
    }
}

/// Soft delete: mark the workspace as deleted and kill any live PTY sessions
/// so they can't keep writing to a worktree we're about to tear down. The
/// hourly purger does the actual git/worktree cleanup once the entry is
/// older than the grace window. Use `cancel_delete_workspace` to undo
/// before the purger runs.
#[tauri::command]
pub async fn delete_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    tmux_bin: State<'_, TmuxBin>,
    id: WorkspaceId,
) -> AppResult<()> {
    let session_ids: Vec<String> = store
        .read(|s| {
            s.find_workspace(&id)
                .map(|w| w.sessions.iter().map(|m| m.id.clone()).collect())
        })
        .await
        .ok_or_else(|| AppError::WorkspaceNotFound(id.clone()))?;

    // Kill tmux sessions so claude processes stop writing to the worktree
    // before the purger removes it. The supervisor reacts to the resulting
    // session:exit and cleans up its own state.
    if !tmux_bin.0.as_os_str().is_empty() {
        for sid in &session_ids {
            tmux::kill_session(&tmux_bin.0, sid);
        }
    }

    let touched = store
        .mutate(|s| {
            let ws = s
                .find_workspace_mut(&id)
                .ok_or_else(|| AppError::WorkspaceNotFound(id.clone()))?;
            // Idempotent: re-deleting an already-soft-deleted workspace
            // refreshes the timestamp, which extends the grace window.
            ws.deleted_at = Some(Utc::now());
            // Archive + delete are mutually exclusive views; clear archive
            // so the entry doesn't double-count if someone unarchives later.
            ws.archived_at = None;
            Ok(())
        })
        .await;
    if let Err(e) = touched {
        return Err(e);
    }

    info!(%id, "soft-deleted workspace");
    emit_workspace_changed(&app, &id);
    let _ = app.emit("system_status:changed", &());
    Ok(())
}

/// Undo a soft delete. Only succeeds if the purger hasn't already
/// reaped the workspace.
#[tauri::command]
pub async fn cancel_delete_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    id: WorkspaceId,
) -> AppResult<()> {
    store
        .mutate(|s| {
            let ws = s
                .find_workspace_mut(&id)
                .ok_or_else(|| AppError::WorkspaceNotFound(id.clone()))?;
            ws.deleted_at = None;
            Ok(())
        })
        .await?;
    emit_workspace_changed(&app, &id);
    let _ = app.emit("system_status:changed", &());
    Ok(())
}

#[tauri::command]
pub async fn archive_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    id: WorkspaceId,
) -> AppResult<()> {
    store
        .mutate(|s| {
            let ws = s
                .find_workspace_mut(&id)
                .ok_or_else(|| AppError::WorkspaceNotFound(id.clone()))?;
            ws.archived_at = Some(Utc::now());
            Ok(())
        })
        .await?;
    emit_workspace_changed(&app, &id);
    Ok(())
}

#[tauri::command]
pub async fn unarchive_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    id: WorkspaceId,
) -> AppResult<()> {
    store
        .mutate(|s| {
            let ws = s
                .find_workspace_mut(&id)
                .ok_or_else(|| AppError::WorkspaceNotFound(id.clone()))?;
            ws.archived_at = None;
            Ok(())
        })
        .await?;
    emit_workspace_changed(&app, &id);
    Ok(())
}

/// Reorder the active workspaces (everything not soft-deleted and not
/// archived). The frontend computes a new ordering by drag-and-drop and
/// posts the resulting ID list. Workspaces not in the list keep their
/// current relative position in `AppState.workspaces`.
#[tauri::command]
pub async fn reorder_workspaces(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    ids: Vec<WorkspaceId>,
) -> AppResult<()> {
    store
        .mutate(|s| {
            // Validate every id exists; bail without mutating on mismatch
            // so a stale frontend snapshot can't shuffle the wrong rows.
            for id in &ids {
                if !s.workspaces.iter().any(|w| &w.id == id) {
                    return Err(AppError::WorkspaceNotFound(id.clone()));
                }
            }
            // Pull the named workspaces out in their requested order.
            let mut moved: Vec<Workspace> = Vec::with_capacity(ids.len());
            for id in &ids {
                if let Some(pos) = s.workspaces.iter().position(|w| &w.id == id) {
                    moved.push(s.workspaces.remove(pos));
                }
            }
            // Re-insert at the front. Archived/soft-deleted entries that
            // weren't included keep their positions after the moved block.
            for ws in moved.into_iter().rev() {
                s.workspaces.insert(0, ws);
            }
            Ok(())
        })
        .await?;
    let _ = app.emit("workspace:reordered", &());
    Ok(())
}

/// Trigger the background purger immediately. Used by the "Run cleanup
/// now" button on the system status page. Still respects the 1-hour
/// grace window — entries deleted under an hour ago stay put.
#[tauri::command]
pub fn run_purge_now(purger: State<'_, Arc<Purger>>) -> AppResult<()> {
    purger.request_tick();
    Ok(())
}

#[tauri::command]
pub async fn list_system_errors(
    store: State<'_, Arc<Store>>,
) -> AppResult<Vec<SystemErrorEntry>> {
    Ok(store.read(|s| s.system_errors.clone()).await)
}

#[tauri::command]
pub async fn dismiss_system_error(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    id: String,
) -> AppResult<()> {
    store
        .mutate(|s| {
            s.system_errors.retain(|e| e.id != id);
            Ok(())
        })
        .await?;
    let _ = app.emit("system_status:changed", &());
    Ok(())
}

#[tauri::command]
pub async fn pause_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    id: WorkspaceId,
) -> AppResult<()> {
    set_paused(&store, &id, true).await?;
    emit_workspace_changed(&app, &id);
    Ok(())
}

#[tauri::command]
pub async fn resume_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    id: WorkspaceId,
) -> AppResult<()> {
    set_paused(&store, &id, false).await?;
    emit_workspace_changed(&app, &id);
    Ok(())
}

#[tauri::command]
pub async fn list_discrepancies(
    store: State<'_, Arc<Store>>,
    registry: State<'_, Arc<RegistryLoad>>,
    in_progress: State<'_, InProgressWorkspaces>,
) -> AppResult<Discrepancies> {
    let snapshot = store.read(|s| s.clone()).await;
    let pending = in_progress.snapshot();
    let reg = match &**registry {
        RegistryLoad::Ok { registry, .. } => Some(registry),
        _ => None,
    };
    Ok(reconcile::scan(&snapshot, reg, &pending).await)
}

/// Delete a directory that the reconciler flagged as orphaned. The path is
/// validated against `worktree_root` to block traversal-style misuse.
#[tauri::command]
pub async fn remove_orphan_dir(
    registry: State<'_, Arc<RegistryLoad>>,
    path: PathBuf,
) -> AppResult<()> {
    let reg = registry.require()?;
    if !reconcile::is_under(&reg.worktree_root, &path) {
        return Err(AppError::Other(format!(
            "refusing to remove {}: not under worktree_root",
            path.display()
        )));
    }
    tokio::fs::remove_dir_all(&path).await?;
    info!(?path, "removed orphaned worktree dir");
    Ok(())
}

/// Drop a workspace from state without running any git ops. Used when a
/// workspace's worktrees are all missing and the user just wants the row
/// gone.
#[tauri::command]
pub async fn forget_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    tmux_bin: State<'_, TmuxBin>,
    id: WorkspaceId,
) -> AppResult<()> {
    let session_ids: Vec<String> = store
        .read(|s| {
            s.find_workspace(&id)
                .map(|w| w.sessions.iter().map(|m| m.id.clone()).collect())
                .unwrap_or_default()
        })
        .await;

    let removed = store
        .mutate(|s| {
            let before = s.workspaces.len();
            s.workspaces.retain(|w| w.id != id);
            Ok(s.workspaces.len() < before)
        })
        .await?;
    if !removed {
        return Err(AppError::WorkspaceNotFound(id));
    }

    // State is gone — kill the tmux sessions too so they don't become
    // orphans reaped on the next boot.
    if !tmux_bin.0.as_os_str().is_empty() {
        for sid in &session_ids {
            tmux::kill_session(&tmux_bin.0, sid);
        }
    }

    info!(%id, "forgot workspace (state-only removal)");
    emit_workspace_changed(&app, &id);
    Ok(())
}

async fn set_paused(store: &Arc<Store>, id: &str, paused: bool) -> AppResult<()> {
    store
        .mutate(|s| {
            let ws = s
                .find_workspace_mut(id)
                .ok_or_else(|| AppError::WorkspaceNotFound(id.to_string()))?;
            ws.paused = paused;
            Ok(())
        })
        .await
}

#[tauri::command]
pub fn list_sessions(
    supervisor: State<'_, Arc<SessionSupervisor>>,
    workspace_id: WorkspaceId,
) -> Vec<SessionInfo> {
    supervisor.list_for_workspace(&workspace_id)
}

#[derive(Debug, serde::Deserialize)]
pub struct StartClaudeArgs {
    pub workspace_id: WorkspaceId,
    /// `None` => start the session at the workspace root (the parent dir
    /// containing each repo's worktree subdir).
    #[serde(default)]
    pub repo_key: Option<String>,
}

/// Spawn a fresh `claude` session in the given workspace/repo worktree.
/// Also writes a `ClaudeSessionMeta` into state with `claude_session_id`
/// left as `None` — it gets filled in by the `SessionStart` hook.
#[tauri::command]
pub async fn start_claude_session(
    app: AppHandle,
    supervisor: State<'_, Arc<SessionSupervisor>>,
    store: State<'_, Arc<Store>>,
    claude_bin: State<'_, ClaudeBin>,
    tmux_bin: State<'_, TmuxBin>,
    args: StartClaudeArgs,
) -> AppResult<SessionInfo> {
    spawn_claude(
        &app,
        &supervisor,
        &store,
        &claude_bin,
        &tmux_bin,
        &args,
        None,
    )
    .await
}

#[derive(Debug, serde::Deserialize)]
pub struct ResumeClaudeArgs {
    pub workspace_id: WorkspaceId,
    /// `None` matches a workspace-root session.
    #[serde(default)]
    pub repo_key: Option<String>,
    /// The `id` field from an existing `ClaudeSessionMeta` — its
    /// `claude_session_id` will be passed to `claude --resume`.
    pub session_meta_id: String,
}

#[tauri::command]
pub async fn resume_claude_session(
    app: AppHandle,
    supervisor: State<'_, Arc<SessionSupervisor>>,
    store: State<'_, Arc<Store>>,
    claude_bin: State<'_, ClaudeBin>,
    tmux_bin: State<'_, TmuxBin>,
    args: ResumeClaudeArgs,
) -> AppResult<SessionInfo> {
    // Pull claude_session_id + cwd from the ClaudeSessionMeta we already
    // persisted on the previous run.
    let lookup = store
        .read(|s| {
            s.find_workspace(&args.workspace_id).and_then(|w| {
                w.sessions
                    .iter()
                    .find(|sess| sess.id == args.session_meta_id)
                    .map(|sess| (sess.claude_session_id.clone(), sess.cwd.clone()))
            })
        })
        .await
        .ok_or_else(|| {
            AppError::Other(format!(
                "no session {} in workspace {}",
                args.session_meta_id, args.workspace_id
            ))
        })?;
    let (claude_sid, cwd) = lookup;

    // If the tmux session from a prior run is still alive, reattach to it
    // — no claude respawn, no transcript replay. The Tethys SessionId is
    // the tmux session name, so we can probe directly.
    if !tmux_bin.0.as_os_str().is_empty()
        && tmux::has_session(&tmux_bin.0, &args.session_meta_id)
    {
        info!(
            session_id = %args.session_meta_id,
            "reattaching to live tmux session"
        );
        let info = supervisor.reattach_tmux(
            args.session_meta_id,
            args.workspace_id.clone(),
            args.repo_key,
            &cwd,
            &tmux_bin.0,
        )?;
        emit_workspace_changed(&app, &args.workspace_id);
        return Ok(info);
    }

    let claude_sid = claude_sid.ok_or_else(|| {
        AppError::Other(
            "session has no claude_session_id yet — resume not possible".into(),
        )
    })?;

    let start = StartClaudeArgs {
        workspace_id: args.workspace_id,
        repo_key: args.repo_key,
    };
    spawn_claude(
        &app,
        &supervisor,
        &store,
        &claude_bin,
        &tmux_bin,
        &start,
        Some(&claude_sid),
    )
    .await
}

async fn spawn_claude(
    app: &AppHandle,
    supervisor: &Arc<SessionSupervisor>,
    store: &Arc<Store>,
    claude_bin: &ClaudeBin,
    tmux_bin: &TmuxBin,
    args: &StartClaudeArgs,
    resume_claude_sid: Option<&str>,
) -> AppResult<SessionInfo> {
    if tmux_bin.0.as_os_str().is_empty() {
        return Err(AppError::Other(
            "tmux not found — install with `brew install tmux` and restart Tethys".into(),
        ));
    }

    // Resolve the cwd: a specific repo's worktree, or — when repo_key is
    // None — the workspace root (parent of every repo worktree).
    // Also pull the per-workspace claude binary override, if any.
    let lookup = store
        .read(|s| {
            let w = s.find_workspace(&args.workspace_id)?;
            let cwd = match args.repo_key.as_deref() {
                Some(key) => w
                    .repo_links
                    .iter()
                    .find(|r| r.repo_key == key)
                    .map(|r| r.worktree_path.clone()),
                None => w
                    .repo_links
                    .first()
                    .and_then(|r| r.worktree_path.parent().map(|p| p.to_path_buf())),
            }?;
            Some((cwd, w.claude_binary.clone()))
        })
        .await
        .ok_or_else(|| {
            AppError::Other(match args.repo_key.as_deref() {
                Some(key) => format!(
                    "no worktree for {}/{} in state",
                    args.workspace_id, key
                ),
                None => format!(
                    "workspace {} has no repos — can't resolve a root dir",
                    args.workspace_id
                ),
            })
        })?;
    let (cwd, ws_binary) = lookup;

    let resolved_bin = match ws_binary.as_deref() {
        Some(bin) => claude::resolve_named(bin)?,
        None => claude_bin.0.clone(),
    };

    let (info, _token) = supervisor.spawn_claude(
        args.workspace_id.clone(),
        args.repo_key.clone(),
        &cwd,
        &tmux_bin.0,
        &resolved_bin,
        resume_claude_sid,
    )?;

    // Persist a ClaudeSessionMeta entry so resume works across restarts.
    // claude_session_id is filled in by the SessionStart hook once it
    // arrives. We key on the Tethys-internal `id` (== SessionSupervisor id)
    // so the UI and supervisor use a shared identifier.
    let meta = ClaudeSessionMeta {
        id: info.id.clone(),
        repo_key: args.repo_key.clone(),
        cwd: cwd.clone(),
        claude_session_id: None,
        transcript_path: None,
        hidden: false,
    };

    store
        .mutate(|s| {
            let ws = s
                .find_workspace_mut(&args.workspace_id)
                .ok_or_else(|| AppError::WorkspaceNotFound(args.workspace_id.clone()))?;
            // Resuming? Drop the prior meta for this Claude conversation so
            // we don't accumulate dormant duplicates with the same
            // claude_session_id across runs.
            if let Some(csid) = resume_claude_sid {
                ws.sessions
                    .retain(|m| m.claude_session_id.as_deref() != Some(csid));
            }
            // Defensive: no dupes of the new tethys id either.
            ws.sessions.retain(|m| m.id != meta.id);
            ws.sessions.push(meta);
            Ok(())
        })
        .await?;

    emit_workspace_changed(app, &args.workspace_id);
    Ok(info)
}

#[derive(Debug, serde::Deserialize)]
pub struct SetClaudeHiddenArgs {
    pub workspace_id: WorkspaceId,
    pub session_id: String,
    pub hidden: bool,
}

/// Toggle a Claude session's `hidden` flag in state. Cosmetic only — the
/// tmux session and the supervisor's `SessionHandle` keep running.
#[tauri::command]
pub async fn set_claude_session_hidden(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    args: SetClaudeHiddenArgs,
) -> AppResult<()> {
    let touched = store
        .mutate(|s| {
            let ws = s
                .find_workspace_mut(&args.workspace_id)
                .ok_or_else(|| AppError::WorkspaceNotFound(args.workspace_id.clone()))?;
            let Some(meta) = ws.sessions.iter_mut().find(|m| m.id == args.session_id) else {
                return Ok(false);
            };
            meta.hidden = args.hidden;
            Ok(true)
        })
        .await?;

    if !touched {
        return Err(AppError::Other(format!(
            "session {} not found in workspace {}",
            args.session_id, args.workspace_id
        )));
    }

    emit_workspace_changed(&app, &args.workspace_id);
    Ok(())
}

/// Newtype so `claude_bin` can be managed in Tauri state.
pub struct ClaudeBin(pub std::path::PathBuf);

/// Subscribe to live PTY bytes and return the current scrollback. The
/// channel carries raw bytes via `InvokeResponseBody::Raw` — no JSON
/// serialization overhead per chunk.
#[tauri::command]
pub fn attach_session(
    supervisor: State<'_, Arc<SessionSupervisor>>,
    session_id: String,
    on_bytes: tauri::ipc::Channel<InvokeResponseBody>,
) -> AppResult<Vec<u8>> {
    supervisor.attach(&session_id, on_bytes)
}

#[tauri::command]
pub fn send_input(
    supervisor: State<'_, Arc<SessionSupervisor>>,
    session_id: String,
    data: Vec<u8>,
) -> AppResult<()> {
    supervisor.send_input(&session_id, &data)?;
    // Turn state is driven by Claude Code's UserPromptSubmit / Stop /
    // Notification hooks — no optimistic flip needed here.
    Ok(())
}

#[tauri::command]
pub fn resize_session(
    supervisor: State<'_, Arc<SessionSupervisor>>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> AppResult<()> {
    supervisor.resize(&session_id, cols, rows)
}

#[tauri::command]
pub fn get_theme(paths: State<'_, Paths>) -> AppResult<Option<Theme>> {
    Theme::load_saved(&paths.theme_file())
}

fn emit_workspace_changed(app: &AppHandle, workspace_id: &str) {
    let _ = app.emit(
        "workspace:changed",
        serde_json::json!({ "workspace_id": workspace_id }),
    );
}

/// Spawn a task that drains an mpsc of `JobEvent` into the Tauri `Channel`.
/// Returns a `JobTx` the orchestrator uses to emit events. Dropping the tx
/// (or returning from the command) closes the mpsc and the forwarder exits.
fn spawn_event_forwarder(channel: Channel<JobEvent>) -> JobTx {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JobEvent>();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if channel.send(event).is_err() {
                break;
            }
        }
    });
    JobTx(tx)
}
