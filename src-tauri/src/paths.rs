use std::path::PathBuf;
use tauri::{AppHandle, Manager};

use crate::error::{AppError, AppResult};

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

    pub fn hook_socket(&self) -> PathBuf {
        self.data_dir.join("hook.sock")
    }

    pub fn claude_settings_lock(&self) -> PathBuf {
        self.data_dir.join("claude-settings.lock")
    }
}

/// `~/.claude/settings.json` — user-level Claude Code settings.
pub fn claude_settings_path() -> Option<PathBuf> {
    Some(dirs_home()?.join(".claude").join("settings.json"))
}

/// Resolve the tethys-hook companion binary next to the current executable.
/// In dev, Cargo places both at `<workspace>/target/debug/`; in a bundled
/// app they'd need to sit side by side too.
pub fn tethys_hook_bin() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let parent = exe.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no parent for current exe")
    })?;
    Ok(parent.join("tethys-hook"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
