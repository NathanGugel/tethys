use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::registry::RepoRegistry;
use crate::state::AppState;

/// Result of diffing `state.json` against what's actually on disk under
/// `worktree_root`. Computed on-demand via `list_discrepancies` and after
/// any workspace mutation.
#[derive(Debug, Default, Serialize)]
pub struct Discrepancies {
    /// Directories under `worktree_root` with no matching workspace in state.
    /// Typically the result of a crash or kill mid-create.
    pub orphaned_dirs: Vec<OrphanedDir>,
    /// Workspaces in state whose worktree paths no longer exist on disk.
    /// Typically the result of someone manually deleting the dir.
    pub missing_worktrees: Vec<MissingWorktree>,
}

#[derive(Debug, Serialize)]
pub struct OrphanedDir {
    pub path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct MissingWorktree {
    pub workspace_id: String,
    pub branch: String,
    pub repo_key: String,
    pub worktree_path: PathBuf,
}

/// Scan the filesystem against AppState. Safe to call any time.
///
/// Without a loaded registry we can't know `worktree_root`, so orphan
/// detection is skipped — we still report missing worktrees based on
/// `AppState.repo_links`.
///
/// `in_progress` is the set of workspace IDs currently being created.
/// Those directories legitimately exist on disk while state.json hasn't
/// been updated yet, so we skip them to avoid a false-positive orphan.
pub async fn scan(
    state: &AppState,
    registry: Option<&RepoRegistry>,
    in_progress: &HashSet<String>,
) -> Discrepancies {
    let mut out = Discrepancies::default();

    // Missing worktrees: state says they should exist, disk says they don't.
    for ws in &state.workspaces {
        for link in &ws.repo_links {
            if !link.worktree_path.exists() {
                out.missing_worktrees.push(MissingWorktree {
                    workspace_id: ws.id.clone(),
                    branch: ws.branch.clone(),
                    repo_key: link.repo_key.clone(),
                    worktree_path: link.worktree_path.clone(),
                });
            }
        }
    }

    // Orphaned dirs: top-level dirs under worktree_root with no matching
    // workspace. We don't inspect per-repo subdirs — workspace-level
    // orphaning is the only partial-state case our create flow produces.
    let Some(reg) = registry else {
        return out;
    };
    let known_ids: HashSet<&str> =
        state.workspaces.iter().map(|w| w.id.as_str()).collect();

    let mut entries = match tokio::fs::read_dir(&reg.worktree_root).await {
        Ok(e) => e,
        Err(_) => return out,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(ft) = entry.file_type().await else { continue };
        if !ft.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if known_ids.contains(name) || in_progress.contains(name) {
            continue;
        }
        out.orphaned_dirs.push(OrphanedDir {
            path: path.clone(),
        });
    }

    out
}

/// Sanity check: ensure `candidate` is a path under `worktree_root`. Used
/// before `rm -rf`ing anything the frontend asked about, so a buggy or
/// malicious caller can't hand us `/` and have a bad day.
pub fn is_under(worktree_root: &Path, candidate: &Path) -> bool {
    let Ok(root) = worktree_root.canonicalize() else {
        return false;
    };
    let Ok(cand) = candidate.canonicalize() else {
        return false;
    };
    cand.starts_with(&root) && cand != root
}
