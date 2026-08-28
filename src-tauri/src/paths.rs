use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

use crate::error::{AppError, AppResult};

#[derive(Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn from_app(app: &AppHandle) -> AppResult<Self> {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| AppError::Other(format!("resolving app data dir: {e}")))?;
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(data_dir.join("logs"))?;
        Ok(Self { data_dir })
    }

    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub fn state_tmp_file(&self) -> PathBuf {
        self.data_dir.join("state.json.tmp")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn repos_config_file(&self) -> PathBuf {
        self.data_dir.join("repos.toml")
    }

    pub fn repos_schema_file(&self) -> PathBuf {
        self.data_dir.join("repos.schema.json")
    }

    pub fn repos_clone_dir(&self) -> PathBuf {
        self.data_dir.join("repos")
    }

    pub fn repo_clone_path(&self, repo_key: &str) -> PathBuf {
        self.repos_clone_dir().join(repo_key)
    }

    pub fn symlinks_dir(&self) -> PathBuf {
        self.data_dir.join("symlinks")
    }

    /// Shared `settings.local.json` for a repo — symlinked into each of that
    /// repo's worktrees so permissions stay in sync across workspaces.
    pub fn repo_shared_claude_local(&self, repo_key: &str) -> PathBuf {
        self.symlinks_dir().join(repo_key).join("settings.local.json")
    }

    pub fn hook_socket(&self) -> PathBuf {
        self.data_dir.join("hook.sock")
    }

    /// Socket the per-session `tethys-mcp` servers dial. Separate from
    /// `hook.sock` because the two have opposite contracts: hook frames are
    /// fire-and-forget, MCP frames wait for an answer.
    pub fn mcp_socket(&self) -> PathBuf {
        self.data_dir.join("mcp.sock")
    }

    pub fn claude_settings_lock(&self) -> PathBuf {
        self.data_dir.join("claude-settings.lock")
    }

    pub fn theme_file(&self) -> PathBuf {
        self.data_dir.join("theme.json")
    }
}

/// `~/.claude/settings.json` — user-level Claude Code settings.
pub fn claude_settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("settings.json"))
}

/// `~/.claude/projects/<escaped cwd>` — where Claude Code keeps the session
/// transcripts for a directory.
///
/// Claude keys these by the working directory with every `/` turned into `-`,
/// which is why `--resume` can't find a session started somewhere else: it
/// looks in the directory belonging to *its* cwd. Copying the transcript into
/// the destination's directory is what lets a session follow a worktree.
pub fn claude_project_dir(cwd: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let escaped = cwd.to_string_lossy().replace('/', "-");
    Some(
        PathBuf::from(home)
            .join(".claude")
            .join("projects")
            .join(escaped),
    )
}

/// Resolve a companion binary sitting next to the current executable. In dev,
/// Cargo places them all at `<workspace>/target/debug/`; in a bundled app they
/// need to sit side by side too.
fn companion_bin(name: &str) -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let parent = exe.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no parent for current exe")
    })?;
    Ok(parent.join(name))
}

pub fn tethys_hook_bin() -> std::io::Result<PathBuf> {
    companion_bin("tethys-hook")
}

pub fn tethys_mcp_bin() -> std::io::Result<PathBuf> {
    companion_bin("tethys-mcp")
}

#[cfg(test)]
mod tests {
    use super::claude_project_dir;
    use std::path::{Path, PathBuf};

    /// The escaping is the whole trick behind moving a session: get it wrong
    /// and `--resume` reports "No conversation found".
    #[test]
    fn a_project_dir_is_the_cwd_with_slashes_turned_into_dashes() {
        std::env::set_var("HOME", "/Users/someone");
        assert_eq!(
            claude_project_dir(Path::new("/Users/someone/newlantern/nl-backend")),
            Some(PathBuf::from(
                "/Users/someone/.claude/projects/-Users-someone-newlantern-nl-backend"
            ))
        );
    }

    /// A dash already in the path is left alone, so a directory containing one
    /// yields a doubled dash rather than being normalised away.
    #[test]
    fn existing_dashes_survive_the_escaping() {
        std::env::set_var("HOME", "/Users/someone");
        assert_eq!(
            claude_project_dir(Path::new("/tmp/a-b/c")),
            Some(PathBuf::from("/Users/someone/.claude/projects/-tmp-a-b-c"))
        );
    }
}
