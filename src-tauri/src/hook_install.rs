use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::error::{AppError, AppResult};

const MARKER: &str = "Tethys session monitor";
const EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-submit"),
    ("PreToolUse", "pre-tool"),
    ("Stop", "stop"),
    // StopFailure fires when a turn dies to an API error — without this
    // the session would hang in Working forever.
    ("StopFailure", "stop-failure"),
    ("Notification", "notify"),
    // PermissionRequest covers sandbox-escape prompts (network / fs) that
    // Notification doesn't fire for. Elicitation covers MCP user-input
    // requests with the same semantics.
    ("PermissionRequest", "permission-request"),
    ("Elicitation", "elicitation"),
];

/// Ensure the Tethys hook entries are present in `settings_path` for all
/// three events we care about. Idempotent: safe to call on every boot.
/// Wrapped in an advisory `flock` on `lock_path` so two Tethys instances
/// racing don't clobber each other.
pub fn install(
    settings_path: &Path,
    lock_path: &Path,
    tethys_hook_bin: &Path,
) -> AppResult<()> {
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    lock_file.lock_exclusive()?;

    let result = install_inner(settings_path, tethys_hook_bin);

    // Release lock regardless. fs2 returns Err if lock was never held; ignore.
    FileExt::unlock(&lock_file).ok();
    drop(lock_file);

    result
}

fn install_inner(settings_path: &Path, tethys_hook_bin: &Path) -> AppResult<()> {
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut value: Value = match std::fs::read_to_string(settings_path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| {
            AppError::Other(format!(
                "~/.claude/settings.json is not valid JSON: {e}"
            ))
        })?,
        _ => json!({}),
    };

    let obj = value
        .as_object_mut()
        .ok_or_else(|| AppError::Other("settings.json root is not an object".into()))?;

    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| AppError::Other("settings.json `hooks` is not an object".into()))?;

    let bin_display = tethys_hook_bin.to_string_lossy().to_string();

    for (event, subcmd) in EVENTS {
        let arr = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                AppError::Other(format!("settings.json `hooks.{event}` is not an array"))
            })?;

        // Drop any stale Tethys entries (matched by the description marker)
        // so a reinstall after a path change doesn't accumulate duplicates.
        arr.retain(|entry| {
            entry.get("description").and_then(Value::as_str) != Some(MARKER)
        });

        arr.push(json!({
            "matcher": "",
            "description": MARKER,
            "hooks": [{
                "type": "command",
                "command": format!("{} {}", bin_display, subcmd),
            }]
        }));
    }

    write_atomic(settings_path, &value)?;

    // Verify: re-read and confirm at least one entry with our marker for
    // each event. Cheap defense against partial writes or concurrent editor
    // clobbers outside our lock.
    verify(settings_path)?;

    info!(path = %settings_path.display(), "installed Tethys Claude Code hooks");
    Ok(())
}

fn write_atomic(settings_path: &Path, value: &Value) -> AppResult<()> {
    let pretty = serde_json::to_vec_pretty(value)?;
    let tmp = settings_path.with_extension("json.tethys.tmp");
    std::fs::write(&tmp, &pretty)?;
    if let Ok(f) = File::open(&tmp) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, settings_path)?;
    Ok(())
}

fn verify(settings_path: &Path) -> AppResult<()> {
    let bytes = std::fs::read(settings_path)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        AppError::Other(format!("post-write verify: invalid JSON: {e}"))
    })?;
    let hooks = value.get("hooks").and_then(Value::as_object);
    for (event, _) in EVENTS {
        let found = hooks
            .and_then(|h| h.get(*event))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter().any(|e| {
                    e.get("description").and_then(Value::as_str) == Some(MARKER)
                })
            })
            .unwrap_or(false);
        if !found {
            warn!(event, "post-write verify: Tethys entry missing");
            return Err(AppError::Other(format!(
                "verify: no Tethys entry for {event} after write"
            )));
        }
    }
    Ok(())
}

pub fn bundled_hook_bin_or_warn() -> Option<PathBuf> {
    match crate::paths::tethys_hook_bin() {
        Ok(p) if p.exists() => Some(p),
        Ok(p) => {
            warn!(
                path = %p.display(),
                "tethys-hook binary not found — hooks won't fire"
            );
            None
        }
        Err(e) => {
            warn!(error = %e, "could not resolve tethys-hook path");
            None
        }
    }
}
