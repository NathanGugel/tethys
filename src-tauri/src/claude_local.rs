//! Shared `.claude/settings.local.json` per repo: one file under
//! `<data_dir>/symlinks/<repo-key>/settings.local.json` is symlinked into
//! every worktree Tethys creates for that repo, so permission edits in any
//! workspace propagate to all of them.
//!
//! For sessions started at the workspace *root* (parent of every repo's
//! worktree subdir), we additionally synthesize a generated
//! `<workspace-root>/.claude/settings.local.json` that union-merges each
//! repo's permission lists. This file is overwritten on every workspace
//! mutation — manual edits at the root level do not survive.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Map, Value};
use tokio::fs;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::job::JobTx;
use crate::paths::Paths;

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

/// Generate `<workspace_root>/.claude/settings.local.json` by union-merging
/// `permissions.allow` / `deny` / `ask` from each repo's shared
/// `settings.local.json`. File-glob entries that start with `./` are
/// rewritten to be relative to the workspace root (prefixed with the repo
/// key, which is also the worktree subdir name).
///
/// Always overwrites — the workspace root file is generated, not authored.
/// Missing or unparseable per-repo files are skipped with a warning.
pub async fn write_workspace_root_settings(
    workspace_root: &Path,
    repo_keys: &[String],
    paths: &Paths,
) -> AppResult<()> {
    if !fs::try_exists(workspace_root).await? {
        return Ok(());
    }

    let mut allow: Vec<String> = Vec::new();
    let mut deny: Vec<String> = Vec::new();
    let mut ask: Vec<String> = Vec::new();
    let mut seen_allow: BTreeSet<String> = BTreeSet::new();
    let mut seen_deny: BTreeSet<String> = BTreeSet::new();
    let mut seen_ask: BTreeSet<String> = BTreeSet::new();

    for repo_key in repo_keys {
        let path = paths.repo_shared_claude_local(repo_key);
        let raw = match fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "failed to read repo shared settings.local.json"
                );
                continue;
            }
        };
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %path.display(),
                    "repo shared settings.local.json is not valid JSON"
                );
                continue;
            }
        };
        let Some(perms) = parsed.get("permissions").and_then(|v| v.as_object()) else {
            continue;
        };

        for field in ["allow", "deny", "ask"] {
            let Some(arr) = perms.get(field).and_then(|v| v.as_array()) else {
                continue;
            };
            let (target, seen) = match field {
                "allow" => (&mut allow, &mut seen_allow),
                "deny" => (&mut deny, &mut seen_deny),
                "ask" => (&mut ask, &mut seen_ask),
                _ => unreachable!(),
            };
            for item in arr {
                let Some(s) = item.as_str() else { continue };
                let rewritten = rewrite_relative_path(s, repo_key);
                if seen.insert(rewritten.clone()) {
                    target.push(rewritten);
                }
            }
        }
    }

    let mut permissions = Map::new();
    if !allow.is_empty() {
        permissions.insert(
            "allow".into(),
            Value::Array(allow.into_iter().map(Value::String).collect()),
        );
    }
    if !deny.is_empty() {
        permissions.insert(
            "deny".into(),
            Value::Array(deny.into_iter().map(Value::String).collect()),
        );
    }
    if !ask.is_empty() {
        permissions.insert(
            "ask".into(),
            Value::Array(ask.into_iter().map(Value::String).collect()),
        );
    }

    let mut root = Map::new();
    root.insert(
        "_generatedBy".into(),
        Value::String("tethys (regenerated on every workspace change)".into()),
    );
    if !permissions.is_empty() {
        root.insert("permissions".into(), Value::Object(permissions));
    }

    let claude_dir = workspace_root.join(".claude");
    fs::create_dir_all(&claude_dir).await?;
    let file_path = claude_dir.join("settings.local.json");
    let mut content = serde_json::to_string_pretty(&Value::Object(root))
        .map_err(|e| AppError::Other(format!("serializing settings.local.json: {e}")))?;
    content.push('\n');
    fs::write(&file_path, content).await?;
    Ok(())
}

/// Rewrite a permission entry's path to be relative to the workspace root
/// instead of the repo worktree. Only touches entries whose argument starts
/// with `./` (e.g. `Read(./src/**)` → `Read(./<repo_key>/src/**)`); leaves
/// `Bash(...)`, `WebFetch(domain:...)`, absolute paths, `~/...`, and
/// argument-less entries (`mcp__...`, `Skill(...)`) untouched.
fn rewrite_relative_path(entry: &str, repo_key: &str) -> String {
    let Some(open) = entry.find('(') else {
        return entry.to_string();
    };
    let Some(close) = entry.rfind(')') else {
        return entry.to_string();
    };
    if close <= open + 1 {
        return entry.to_string();
    }
    let inside = &entry[open + 1..close];
    let Some(rest) = inside.strip_prefix("./") else {
        return entry.to_string();
    };
    format!(
        "{}(./{}/{}){}",
        &entry[..open],
        repo_key,
        rest,
        &entry[close + 1..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_relative_only_touches_dot_slash() {
        assert_eq!(
            rewrite_relative_path("Read(./src/**)", "frontend"),
            "Read(./frontend/src/**)"
        );
        assert_eq!(
            rewrite_relative_path("Bash(yarn test:*)", "frontend"),
            "Bash(yarn test:*)"
        );
        assert_eq!(
            rewrite_relative_path("WebFetch(domain:github.com)", "frontend"),
            "WebFetch(domain:github.com)"
        );
        assert_eq!(
            rewrite_relative_path("Read(//Users/ryan/x/**)", "frontend"),
            "Read(//Users/ryan/x/**)"
        );
        assert_eq!(
            rewrite_relative_path("Read(~/Downloads/**)", "frontend"),
            "Read(~/Downloads/**)"
        );
        assert_eq!(
            rewrite_relative_path("mcp__linear__get_issue", "frontend"),
            "mcp__linear__get_issue"
        );
        assert_eq!(
            rewrite_relative_path("Skill(see-data)", "frontend"),
            "Skill(see-data)"
        );
    }

    #[tokio::test]
    async fn merges_dedupes_and_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let paths = Paths {
            data_dir: data_dir.clone(),
        };

        let frontend_settings = paths.repo_shared_claude_local("frontend");
        let backend_settings = paths.repo_shared_claude_local("backend");
        fs::create_dir_all(frontend_settings.parent().unwrap())
            .await
            .unwrap();
        fs::create_dir_all(backend_settings.parent().unwrap())
            .await
            .unwrap();
        fs::write(
            &frontend_settings,
            r#"{"permissions":{"allow":["Bash(grep:*)","Read(./src/**)"],"deny":["Bash(rm:*)"]}}"#,
        )
        .await
        .unwrap();
        fs::write(
            &backend_settings,
            r#"{"permissions":{"allow":["Bash(grep:*)","Bash(pytest:*)"]}}"#,
        )
        .await
        .unwrap();

        let workspace_root = data_dir.join("ws");
        fs::create_dir_all(&workspace_root).await.unwrap();
        write_workspace_root_settings(
            &workspace_root,
            &["frontend".into(), "backend".into()],
            &paths,
        )
        .await
        .unwrap();

        let written =
            fs::read_to_string(workspace_root.join(".claude/settings.local.json"))
                .await
                .unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        let allow = parsed["permissions"]["allow"].as_array().unwrap();
        let allow_strs: Vec<&str> = allow.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            allow_strs,
            vec![
                "Bash(grep:*)",
                "Read(./frontend/src/**)",
                "Bash(pytest:*)",
            ],
            "dedupes Bash(grep:*) across repos and rewrites ./ paths"
        );
        let deny = parsed["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny.len(), 1);
        assert_eq!(deny[0].as_str(), Some("Bash(rm:*)"));
        assert!(!parsed["_generatedBy"].as_str().unwrap_or("").is_empty());
    }

    #[tokio::test]
    async fn skips_when_workspace_root_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            data_dir: tmp.path().to_path_buf(),
        };
        let missing = tmp.path().join("does-not-exist");
        write_workspace_root_settings(&missing, &["any".into()], &paths)
            .await
            .unwrap();
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn missing_per_repo_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            data_dir: tmp.path().to_path_buf(),
        };
        let workspace_root = tmp.path().join("ws");
        fs::create_dir_all(&workspace_root).await.unwrap();
        write_workspace_root_settings(
            &workspace_root,
            &["never-symlinked".into()],
            &paths,
        )
        .await
        .unwrap();
        let written =
            fs::read_to_string(workspace_root.join(".claude/settings.local.json"))
                .await
                .unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        // No permissions block when nothing was found.
        assert!(parsed.get("permissions").is_none());
    }
}
