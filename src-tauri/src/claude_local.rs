//! Shared `.claude/settings.local.json` per repo: one file under
//! `<data_dir>/symlinks/<repo-key>/settings.local.json` is symlinked into
//! every worktree Tethys creates for that repo, so permission edits in any
//! workspace propagate to all of them.

use std::path::Path;

use tokio::fs;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::job::JobTx;

const EMPTY_SETTINGS: &str = "{}\n";

/// Ensure `<worktree>/.claude/settings.local.json` is a symlink to
/// `shared_path`, creating the shared file (with `{}`) if it's the first
/// worktree to touch it. If the worktree already has a real file there
/// (e.g. the repo tracks one), leave it alone and warn — replacing it
/// would show up as a git modification and discard committed content.
pub async fn install_symlink(
    worktree_path: &Path,
    shared_path: &Path,
    tx: &JobTx,
    repo_key: &str,
) -> AppResult<()> {
    if let Some(parent) = shared_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    if !fs::try_exists(shared_path).await? {
        fs::write(shared_path, EMPTY_SETTINGS).await?;
    }

    let claude_dir = worktree_path.join(".claude");
    fs::create_dir_all(&claude_dir).await?;
    let link_path = claude_dir.join("settings.local.json");

    match fs::symlink_metadata(&link_path).await {
        Ok(_) => {
            warn!(
                path = %link_path.display(),
                "settings.local.json already exists in worktree; skipping symlink"
            );
            tx.status(
                "settings.local.json already present; leaving as-is",
                Some(repo_key),
            );
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::Io(e)),
    }

    fs::symlink(shared_path, &link_path).await?;
    tx.status(
        format!(
            "linked .claude/settings.local.json -> {}",
            shared_path.display()
        ),
        Some(repo_key),
    );
    Ok(())
}
