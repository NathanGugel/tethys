use std::path::PathBuf;
use std::process::Command;

use tracing::{info, warn};

use crate::error::{AppError, AppResult};

/// Resolve the absolute path to the `claude` binary by running
/// `/bin/zsh -ilc 'which claude'`. Desktop apps on macOS inherit a minimal
/// `$PATH` (no nvm/volta/homebrew dirs), so this is how we reliably find
/// whatever the user has on their login shell `PATH`.
///
/// Called once at boot and cached; re-resolve manually if the user moves
/// their install.
pub fn resolve() -> AppResult<PathBuf> {
    let output = Command::new("/bin/zsh")
        .args(["-ilc", "which claude"])
        .output()
        .map_err(|e| AppError::Other(format!("failed to invoke /bin/zsh: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AppError::Other(format!(
            "`which claude` via /bin/zsh failed: {}",
            if stderr.is_empty() { "no stderr" } else { &stderr }
        )));
    }

    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let path = extract_path(&raw);

    // zsh's `which` can print "claude not found" to stdout under some
    // configurations; a valid absolute path must start with `/`.
    if path.is_empty() || !path.starts_with('/') {
        warn!(?path, "claude not on login-shell PATH");
        return Err(AppError::Other(
            "claude not found — install it and make sure `which claude` works in a login shell".into(),
        ));
    }

    info!(%path, "resolved claude binary");
    Ok(PathBuf::from(path))
}

/// Pull the actual command output from `which claude` after shell-integration
/// noise. iTerm2 + zsh interactive mode prepends OSC escapes (ending in BEL
/// `\x07`) before stdout gets piped to us — everything before the final BEL
/// is preamble, not the path we want.
fn extract_path(raw: &str) -> String {
    let trimmed = match raw.rfind('\x07') {
        Some(idx) => &raw[idx + 1..],
        None => raw,
    };
    trimmed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::extract_path;

    #[test]
    fn plain_output() {
        assert_eq!(extract_path("/usr/local/bin/claude\n"), "/usr/local/bin/claude");
    }

    #[test]
    fn iterm_osc_prefix() {
        let raw = "\x1b]1337;RemoteHost=ryan@host\x07\x1b]1337;CurrentDir=/cwd\x07/Users/ryan/.local/bin/claude\n";
        assert_eq!(extract_path(raw), "/Users/ryan/.local/bin/claude");
    }
}
