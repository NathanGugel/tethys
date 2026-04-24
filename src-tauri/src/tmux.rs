use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::{info, warn};

use crate::error::{AppError, AppResult};

/// Socket label for the tmux server Tethys uses. Kept distinct from the
/// user's personal tmux so their `~/.tmux.conf` isn't loaded, their
/// keybindings don't collide, and we can query/kill our own server without
/// touching their setup.
pub const SOCKET_LABEL: &str = "tethys";

/// Newtype managed in Tauri state, like `ClaudeBin`.
pub struct TmuxBin(pub PathBuf);

/// Resolve the absolute path to `tmux` via a login shell, mirroring
/// `claude::resolve` — desktop apps on macOS don't inherit Homebrew's bin
/// dir in PATH, so we shell out to zsh.
pub fn resolve() -> AppResult<PathBuf> {
    let output = Command::new("/bin/zsh")
        .args(["-ilc", "which tmux"])
        .output()
        .map_err(|e| AppError::Other(format!("failed to invoke /bin/zsh: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AppError::Other(format!(
            "`which tmux` via /bin/zsh failed: {}",
            if stderr.is_empty() { "no stderr" } else { &stderr }
        )));
    }

    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let path = extract_path(&raw);

    if path.is_empty() || !path.starts_with('/') {
        warn!(?path, "tmux not on login-shell PATH");
        return Err(AppError::Other(
            "tmux not found — install with `brew install tmux` and make sure `which tmux` works in a login shell".into(),
        ));
    }

    info!(%path, "resolved tmux binary");
    Ok(PathBuf::from(path))
}

/// `true` if the tmux session named `session_id` exists on the Tethys
/// server. Uses `tmux -L tethys has-session -t <id>` — exit 0 = exists.
pub fn has_session(tmux_bin: &Path, session_id: &str) -> bool {
    Command::new(tmux_bin)
        .args(["-L", SOCKET_LABEL, "has-session", "-t", session_id])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Server-global options, as a tmux multi-command prefix. Prepending
/// these (separated by `;`) to `new-session`/`attach-session` ensures the
/// options apply on cold-start too — at boot the server may not exist,
/// so `set-option` before `new-session` runs against the just-started
/// server.
///
/// - `window-size latest` sizes windows to the most-recently-attached
///   client rather than the min across clients. With a single Tethys
///   client this matches the terminal exactly.
/// - `status off` hides tmux's status bar — Tethys' UI already shows
///   session info, so the bar is just a wasted row of screen.
/// - `mouse on` — without it tmux forwards wheel events as arrow keys
///   when the pane is on the alternate screen. With it, wheel-up enters
///   copy-mode and scrolls tmux's scrollback.
pub fn server_init_args() -> Vec<String> {
    // Each inner slice is one tmux command; `;` in between is tmux's
    // command-chain separator. Tmux is purely a process keeper here —
    // xterm.js owns scrollback, selection, and mouse handling. Keeping
    // tmux's `mouse` at the default (off) lets wheel events pass through
    // to xterm.js for native scrolling of its own buffer.
    let commands: &[&[&str]] = &[
        &["set-option", "-g", "window-size", "latest"],
        &["set-option", "-g", "status", "off"],
        // Explicitly off — a previous run may have turned it on, and the
        // tmux server survives across Tethys restarts.
        &["set-option", "-g", "mouse", "off"],
        // `capture-pane -S -` is bounded by history-limit; bump it so
        // cross-restart reattach has plenty of history to replay into
        // xterm.js.
        &["set-option", "-g", "history-limit", "50000"],
        // Strip alt-screen (smcup/rmcup) from every terminal's terminfo.
        // Without this, tmux-the-client enters alt-screen on xterm.js,
        // which flips xterm.js into its alternate buffer — and xterm's
        // default wheel behavior in alt-buffer is "send arrow keys," not
        // scroll the main-buffer scrollback. Neutering alt-screen keeps
        // everything in the main buffer so wheel events scroll natively.
        &[
            "set-option", "-ga", "terminal-overrides",
            ",*:smcup@:rmcup@",
        ],
    ];
    let mut out = Vec::new();
    for cmd in commands {
        out.extend(cmd.iter().map(|s| s.to_string()));
        out.push(";".into());
    }
    out
}

/// Boot-time best-effort: apply server options if the server is already
/// up from a prior run. Safe to skip failures — `server_init_args()` is
/// also prepended to every spawn, which covers cold-start.
pub fn ensure_server_init(tmux_bin: &Path) {
    let _ = Command::new(tmux_bin)
        .args(["-L", SOCKET_LABEL])
        .args(server_init_args())
        .status();
}

/// Dump a session's pane scrollback + visible buffer, with SGR
/// preserved. Returns `None` if the session doesn't exist or the command
/// fails. Output has `\n` converted to `\r\n` for xterm.js, and an SGR
/// reset appended so lingering attributes don't bleed into the client's
/// redraw.
pub fn capture_pane(tmux_bin: &Path, session_id: &str) -> Option<Vec<u8>> {
    let output = Command::new(tmux_bin)
        .args([
            "-L",
            SOCKET_LABEL,
            "capture-pane",
            "-p",       // print to stdout (don't save to buffer)
            "-e",       // preserve escape sequences (SGR)
            "-S", "-",  // start at top of history
            "-t", session_id,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut out = Vec::with_capacity(output.stdout.len() + 8);
    for &b in &output.stdout {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    // Reset SGR so tmux's first real paint starts from a known state.
    out.extend_from_slice(b"\x1b[0m");
    Some(out)
}

/// Return the names of all sessions on the Tethys tmux server. Empty vec
/// if the server isn't running or has no sessions.
pub fn list_sessions(tmux_bin: &Path) -> Vec<String> {
    let output = Command::new(tmux_bin)
        .args([
            "-L",
            SOCKET_LABEL,
            "list-sessions",
            "-F",
            "#{session_name}",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Best-effort kill of a tmux session by name. Silent on failure.
pub fn kill_session(tmux_bin: &Path, session_id: &str) {
    let _ = Command::new(tmux_bin)
        .args(["-L", SOCKET_LABEL, "kill-session", "-t", session_id])
        .status();
}

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
        assert_eq!(extract_path("/opt/homebrew/bin/tmux\n"), "/opt/homebrew/bin/tmux");
    }

    #[test]
    fn iterm_osc_prefix() {
        let raw = "\x1b]1337;RemoteHost=ryan@host\x07/usr/local/bin/tmux\n";
        assert_eq!(extract_path(raw), "/usr/local/bin/tmux");
    }
}
