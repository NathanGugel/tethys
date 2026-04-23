use std::sync::Arc;

use chrono::Utc;
use serde::Deserialize;
use tauri::{ipc::Channel, AppHandle, Emitter, State};
use tracing::{info, warn};

use std::path::PathBuf;

use tauri::ipc::InvokeResponseBody;

use crate::error::{AppError, AppResult};
use crate::git;
use crate::job::{JobEvent, JobTx};
use crate::paths::Paths;
use crate::reconcile::{self, Discrepancies};
use crate::registry::{starter_template, RegistryLoad, Repo};
use crate::sessions::{SessionInfo, SessionSupervisor};
use crate::setup;
use crate::state::{new_workspace_id, RepoLink, Workspace, WorkspaceId};
use crate::store::Store;

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

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceArgs {
    pub branch: String,
    pub repo_selections: Vec<String>,
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
    let tx = spawn_event_forwarder(on_event);

    let orchestrate = async {
        let mut created: Vec<RepoLink> = Vec::new();
        for repo in &selected {
            let clone_path = paths.repo_clone_path(&repo.key);
            let worktree_path = reg.plan_worktree_path(&id, &repo.key);

            git::ensure_clone(&clone_path, &repo.remote_url, &tx, &repo.key).await?;
            git::worktree_add(&clone_path, &worktree_path, &branch, &tx, &repo.key).await?;

            let mut link = RepoLink {
                repo_key: repo.key.clone(),
                worktree_path: worktree_path.clone(),
                setup_script_ran_at: None,
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
            // We know the planned paths for every selected repo; some may
            // not exist (failure hit before that repo ran). Iterate all
            // in reverse and force-remove whatever's present.
            for repo in selected.iter().rev() {
                let clone_path = paths.repo_clone_path(&repo.key);
                let worktree_path = reg.plan_worktree_path(&id, &repo.key);
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
            }

            let _ = tx.0.send(JobEvent::Failed { error: msg });
            Err(e)
        }
    }
}

#[tauri::command]
pub async fn delete_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    paths: State<'_, Paths>,
    id: WorkspaceId,
    on_event: Channel<JobEvent>,
) -> AppResult<()> {
    let workspace = store
        .read(|s| s.find_workspace(&id).cloned())
        .await
        .ok_or_else(|| AppError::WorkspaceNotFound(id.clone()))?;

    let tx = spawn_event_forwarder(on_event);

    for link in &workspace.repo_links {
        let clone_path = paths.repo_clone_path(&link.repo_key);

        if !link.worktree_path.exists() {
            tx.status(
                format!("worktree {} already gone", link.worktree_path.display()),
                Some(&link.repo_key),
            );
            continue;
        }
        if !clone_path.exists() {
            // Registry entry is gone or the clone was manually deleted.
            tx.status(
                format!(
                    "clone for {} missing; removing worktree dir directly",
                    link.repo_key
                ),
                Some(&link.repo_key),
            );
            if let Err(e) = tokio::fs::remove_dir_all(&link.worktree_path).await {
                let msg = format!("failed to remove {}: {e}", link.worktree_path.display());
                let _ = tx.0.send(JobEvent::Failed { error: msg.clone() });
                return Err(AppError::Other(msg));
            }
            continue;
        }

        if let Err(e) = git::worktree_remove(
            &clone_path,
            &link.worktree_path,
            false, // M3: non-force; dirty-force comes in M7
            &tx,
            &link.repo_key,
        )
        .await
        {
            let _ = tx.0.send(JobEvent::Failed { error: e.to_string() });
            return Err(e);
        }
    }

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

    info!(%id, "deleted workspace");
    let _ = tx.0.send(JobEvent::Success);
    emit_workspace_changed(&app, &id);
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
) -> AppResult<Discrepancies> {
    let snapshot = store.read(|s| s.clone()).await;
    let reg = match &**registry {
        RegistryLoad::Ok { registry, .. } => Some(registry),
        _ => None,
    };
    Ok(reconcile::scan(&snapshot, reg).await)
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
    id: WorkspaceId,
) -> AppResult<()> {
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
pub struct StartSessionArgs {
    pub workspace_id: WorkspaceId,
    pub repo_key: String,
}

#[tauri::command]
pub async fn start_session(
    supervisor: State<'_, Arc<SessionSupervisor>>,
    store: State<'_, Arc<Store>>,
    args: StartSessionArgs,
) -> AppResult<SessionInfo> {
    // Look up the worktree path for this workspace + repo from state.
    let worktree_path = store
        .read(|s| {
            s.find_workspace(&args.workspace_id)
                .and_then(|w| {
                    w.repo_links
                        .iter()
                        .find(|r| r.repo_key == args.repo_key)
                        .map(|r| r.worktree_path.clone())
                })
        })
        .await
        .ok_or_else(|| {
            AppError::Other(format!(
                "no worktree for {}/{} in state",
                args.workspace_id, args.repo_key
            ))
        })?;

    // M4: spawn the user's login shell. M5 will swap this for `claude`.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());

    supervisor.spawn(args.workspace_id, args.repo_key, &worktree_path, &shell)
}

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
    supervisor.send_input(&session_id, &data)
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
