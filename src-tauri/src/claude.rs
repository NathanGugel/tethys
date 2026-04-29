use std::path::PathBuf;
use std::process::Command;

use tracing::{info, warn};

use crate::error::{AppError, AppResult};
use crate::shell::extract_path;

/// Resolve the absolute path to the `claude` binary by running
/// `/bin/zsh -ilc 'which claude'`. Desktop apps on macOS inherit a minimal
/// `$PATH` (no nvm/volta/homebrew dirs), so this is how we reliably find
/// whatever the user has on their login shell `PATH`.
///
/// Called once at boot and cached; re-resolve manually if the user moves
/// their install.
pub fn resolve() -> AppResult<PathBuf> {
    resolve_named("claude")
}

/// Like `resolve` but for an arbitrary entry-point name (e.g. `claude-hipaa`),
/// so per-workspace binary overrides can use the same login-shell PATH lookup.
pub fn resolve_named(bin: &str) -> AppResult<PathBuf> {
    if bin.is_empty() || bin.contains(|c: char| c.is_whitespace() || c == '\'' || c == '"') {
        return Err(AppError::Other(format!(
            "invalid claude binary name: {bin:?}"
        )));
    }
    let cmd = format!("which {bin}");
    let output = Command::new("/bin/zsh")
        .args(["-ilc", &cmd])
        .output()
        .map_err(|e| AppError::Other(format!("failed to invoke /bin/zsh: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AppError::Other(format!(
            "`which {bin}` via /bin/zsh failed: {}",
            if stderr.is_empty() { "no stderr" } else { &stderr }
        )));
    }

    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let path = extract_path(&raw);

    if path.is_empty() || !path.starts_with('/') {
        warn!(?path, %bin, "binary not on login-shell PATH");
        return Err(AppError::Other(format!(
            "{bin} not found — install it and make sure `which {bin}` works in a login shell"
        )));
    }

    info!(%path, %bin, "resolved claude binary");
    Ok(PathBuf::from(path))
}

