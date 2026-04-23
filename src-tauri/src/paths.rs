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
}
