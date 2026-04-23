use std::sync::Arc;

use chrono::Utc;
use serde::Deserialize;
use tauri::{AppHandle, Emitter, State};
use tracing::info;

use crate::error::{AppError, AppResult};
use crate::paths::Paths;
use crate::registry::{starter_template, RegistryLoad, Repo};
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

/// Ensure `repos.toml` exists (writing a starter template if not), then open it
/// with the OS default handler so the user can edit it in their editor of choice.
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
    /// Registry keys of the repos to include in this workspace. Must be non-empty.
    pub repo_selections: Vec<String>,
}

#[tauri::command]
pub async fn create_workspace(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    registry: State<'_, Arc<RegistryLoad>>,
    args: CreateWorkspaceArgs,
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

    // Validate every selected key against the registry before mutating state.
    let selected: Vec<&Repo> = args
        .repo_selections
        .iter()
        .map(|key| {
            reg.find_repo(key).ok_or_else(|| {
                AppError::Other(format!("unknown repo key: {key}"))
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    let id = new_workspace_id();
    let repo_links: Vec<RepoLink> = selected
        .iter()
        .map(|r| RepoLink {
            repo_key: r.key.clone(),
            worktree_path: reg.plan_worktree_path(&id, &r.key),
            setup_script_ran_at: None,
        })
        .collect();

    let workspace = Workspace {
        id,
        branch,
        paused: false,
        created_at: Utc::now(),
        repo_links,
        sessions: Vec::new(),
    };

    let created = store
        .mutate(|s| {
            s.workspaces.push(workspace.clone());
            Ok(workspace.clone())
        })
        .await?;

    info!(
        id = %created.id,
        branch = %created.branch,
        repos = created.repo_links.len(),
        "created workspace",
    );
    emit_workspace_changed(&app, &created.id);
    Ok(created)
}

#[tauri::command]
pub async fn delete_workspace(
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

    info!(%id, "deleted workspace");
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

fn emit_workspace_changed(app: &AppHandle, workspace_id: &str) {
    let _ = app.emit(
        "workspace:changed",
        serde_json::json!({ "workspace_id": workspace_id }),
    );
}
