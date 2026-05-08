//! iTerm2 `.itermcolors` loader + persisted app theme.
//!
//! An `.itermcolors` file is an Apple plist with entries like:
//!   <key>Ansi 0 Color</key>
//!   <dict>
//!     <key>Red Component</key><real>0.27...</real>
//!     <key>Green Component</key><real>0.27...</real>
//!     <key>Blue Component</key><real>0.35...</real>
//!     ...
//!   </dict>
//!
//! We pull the fields we care about, convert sRGB floats to `#rrggbb` hex, and
//! persist the normalized form as `theme.json` so the original file can move
//! or disappear without breaking Tethys.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tracing::info;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    /// Display name — the file stem of the `.itermcolors` file.
    pub name: String,
    /// Original path the user picked. Informational; we don't re-read it.
    pub source_path: PathBuf,
    pub colors: ThemeColors,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeColors {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub cursor_text: String,
    pub selection: String,
    /// 16 ANSI colors (`ansi[0]` = black, `ansi[1]` = red, ...).
    pub ansi: [String; 16],
}

impl Theme {
    pub fn load_from_file(path: &Path) -> AppResult<Self> {
        let dict: BTreeMap<String, plist::Value> =
            plist::from_file(path).map_err(|e| {
                AppError::Other(format!("parsing {}: {e}", path.display()))
            })?;

        let get = |key: &str| -> AppResult<String> {
            let entry = dict
                .get(key)
                .ok_or_else(|| AppError::Other(format!("missing `{key}` in {}", path.display())))?;
            plist_color_to_hex(entry).ok_or_else(|| {
                AppError::Other(format!("`{key}` is not a color dict in {}", path.display()))
            })
        };

        let mut ansi: [String; 16] = Default::default();
        for (i, slot) in ansi.iter_mut().enumerate() {
            *slot = get(&format!("Ansi {i} Color"))?;
        }

        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "theme".into());

        Ok(Theme {
            name,
            source_path: path.to_path_buf(),
            colors: ThemeColors {
                background: get("Background Color")?,
                foreground: get("Foreground Color")?,
                cursor: get("Cursor Color")?,
                cursor_text: get("Cursor Text Color")?,
                selection: get("Selection Color")?,
                ansi,
            },
        })
    }

    pub fn save(&self, path: &Path) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load_saved(path: &Path) -> AppResult<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(path)?;
        let theme: Theme = serde_json::from_slice(&bytes)?;
        Ok(Some(theme))
    }
}

/// Parse the given `.itermcolors` file, persist it to `save_path`, and emit
/// `theme:changed` so the frontend can re-style. Shared by the View menu
/// handler and any future command/programmatic caller.
pub fn load_and_emit(
    app: &AppHandle,
    source: &Path,
    save_path: &Path,
) -> AppResult<Theme> {
    let theme = Theme::load_from_file(source)?;
    theme.save(save_path)?;
    info!(name = %theme.name, source = %source.display(), "loaded theme");
    let _ = app.emit("theme:changed", &theme);
    Ok(theme)
}

pub fn clear_and_emit(app: &AppHandle, save_path: &Path) -> AppResult<()> {
    if save_path.exists() {
        std::fs::remove_file(save_path)?;
    }
    info!("cleared theme");
    let _ = app.emit("theme:changed", serde_json::Value::Null);
    Ok(())
}

/// Convert `{ Red Component, Green Component, Blue Component }` (sRGB floats
/// 0.0–1.0) to `#rrggbb`. Returns `None` if the plist entry isn't a dict or
/// any of the expected component keys are missing.
fn plist_color_to_hex(val: &plist::Value) -> Option<String> {
    let dict = val.as_dictionary()?;
    let c = |key: &str| -> Option<f64> {
        let v = dict.get(key)?;
        v.as_real().or_else(|| v.as_signed_integer().map(|n| n as f64))
    };
    let r = c("Red Component")?;
    let g = c("Green Component")?;
    let b = c("Blue Component")?;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        clamp_byte(r),
        clamp_byte(g),
        clamp_byte(b)
    ))
}

fn clamp_byte(f: f64) -> u8 {
    (f.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_and_rounds() {
        assert_eq!(clamp_byte(0.0), 0);
        assert_eq!(clamp_byte(1.0), 255);
        assert_eq!(clamp_byte(0.5), 128);
        assert_eq!(clamp_byte(-0.1), 0);
        assert_eq!(clamp_byte(1.1), 255);
    }
}
