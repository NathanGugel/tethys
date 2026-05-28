use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::error::{AppError, AppResult};
use crate::job::{JobTx, LogStream};

/// Run a child process, streaming each line of stdout/stderr as `JobEvent::Log`
/// via the provided `JobTx`. Blocks until the child exits. Returns the exit
/// status so the caller decides what to do on non-zero.
///
/// `repo` is attached to each emitted event so the UI can group output by repo.
pub async fn run_streamed<I, S>(
    program: &str,
    args: I,
    cwd: Option<&Path>,
    tx: &JobTx,
    repo: Option<&str>,
) -> AppResult<std::process::ExitStatus>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.env("GIT_TERMINAL_PROMPT", "0"); // fail fast instead of hanging on auth prompt

    let mut child = cmd.spawn().map_err(|e| {
        AppError::Other(format!("failed to spawn `{program}`: {e}"))
    })?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let tx_out = tx.clone();
    let repo_out = repo.map(String::from);
    let stdout_task = tokio::spawn(async move {
        drain_lines(stdout, &tx_out, LogStream::Stdout, repo_out.as_deref()).await;
    });

    let tx_err = tx.clone();
    let repo_err = repo.map(String::from);
    let stderr_task = tokio::spawn(async move {
        drain_lines(stderr, &tx_err, LogStream::Stderr, repo_err.as_deref()).await;
    });

    let status = child.wait().await?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    Ok(status)
}

/// Probe whether `clone_path` looks like a complete git clone by asking
/// `git rev-parse HEAD`. A half-finished clone (process killed after `.git/`
/// was created but before HEAD was written) fails this check.
async fn is_valid_clone(clone_path: &Path) -> bool {
    let result = Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .arg("rev-parse")
        .arg("--verify")
        .arg("HEAD")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    matches!(result, Ok(s) if s.success())
}

/// Read from `reader`, split on both `\n` and `\r` (git/yarn/pnpm progress
/// overwrites the current line with `\r` alone), and emit each segment as
/// a `JobEvent::Log`. Without splitting on `\r`, progress lines never
/// surface — the user just sees "Cloning into..." and then nothing for
/// minutes while the clone runs.
async fn drain_lines<R: AsyncRead + Unpin>(
    mut reader: R,
    tx: &JobTx,
    stream: LogStream,
    repo: Option<&str>,
) {
    let mut buf = [0u8; 4096];
    let mut line: Vec<u8> = Vec::with_capacity(256);
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break, // EOF
            Ok(n) => {
                for &byte in &buf[..n] {
                    if byte == b'\n' || byte == b'\r' {
                        if !line.is_empty() {
                            tx.log(
                                stream,
                                String::from_utf8_lossy(&line).into_owned(),
                                repo,
                            );
                            line.clear();
                        }
                    } else {
                        line.push(byte);
                    }
                }
            }
            Err(_) => break,
        }
    }
    if !line.is_empty() {
        tx.log(stream, String::from_utf8_lossy(&line).into_owned(), repo);
    }
}

/// Clone `remote_url` into `clone_path` if it's not already a valid clone.
/// Partial/broken clones (e.g. from a previous run that was interrupted
/// mid-fetch) are detected via a `git rev-parse HEAD` probe and wiped so
/// the re-clone can succeed — otherwise `git clone` refuses to write into
/// a non-empty directory.
pub async fn ensure_clone(
    clone_path: &Path,
    remote_url: &str,
    tx: &JobTx,
    repo: &str,
) -> AppResult<()> {
    if clone_path.exists() {
        if is_valid_clone(clone_path).await {
            tx.status(
                format!("clone already present at {}", clone_path.display()),
                Some(repo),
            );
            return Ok(());
        }
        tx.status(
            format!(
                "clone at {} is incomplete; removing and retrying",
                clone_path.display()
            ),
            Some(repo),
        );
        tokio::fs::remove_dir_all(clone_path).await.map_err(|e| {
            AppError::Other(format!(
                "failed to remove broken clone at {}: {e}",
                clone_path.display()
            ))
        })?;
    }

    tx.status(format!("cloning {remote_url}"), Some(repo));

    if let Some(parent) = clone_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let status = run_streamed(
        "git",
        [
            "clone".as_ref(),
            // Force progress output even when stderr is a pipe (default is
            // to suppress). Without this users see only "Cloning into..."
            // and then nothing for the duration of a multi-minute clone.
            "--progress".as_ref(),
            remote_url.as_ref(),
            clone_path.as_os_str(),
        ],
        None,
        tx,
        Some(repo),
    )
    .await?;

    if !status.success() {
        return Err(AppError::Other(format!(
            "git clone {remote_url} exited with {:?}",
            status.code()
        )));
    }
    Ok(())
}

/// `git -C <clone_path> pull --ff-only`. Tethys never modifies the clone's
/// working tree or checked-out branch, so a fast-forward pull should always
/// succeed when online. A failure means the clone is in a bad state (dirty
/// working tree, diverged history) and branching off it would silently use
/// stale code — bubble the error so workspace creation aborts loudly.
pub async fn pull_clone(clone_path: &Path, tx: &JobTx, repo: &str) -> AppResult<()> {
    tx.status("updating clone from origin".to_string(), Some(repo));
    let args: [&OsStr; 4] = [
        "-C".as_ref(),
        clone_path.as_os_str(),
        "pull".as_ref(),
        "--ff-only".as_ref(),
    ];
    let status = run_streamed("git", args, None, tx, Some(repo)).await?;
    if !status.success() {
        return Err(AppError::Other(format!(
            "git pull --ff-only in {} exited with {:?}",
            clone_path.display(),
            status.code()
        )));
    }
    Ok(())
}

/// Creates a new branch `<branch>` and a worktree checking it out.
///
/// With `track_from = None`: `git worktree add <worktree_path> -b <branch>` —
/// new branch starts at the clone's current HEAD with no upstream.
///
/// With `track_from = Some("origin/<branch>")`:
/// `git worktree add --track -b <branch> <worktree_path> origin/<branch>` —
/// new branch starts at the remote ref and is set to track it. Used when the
/// caller has already verified the remote branch exists, so the worktree
/// lands on the remote's commit with upstream wired up in one step.
pub async fn worktree_add(
    clone_path: &Path,
    worktree_path: &Path,
    branch: &str,
    track_from: Option<&str>,
    tx: &JobTx,
    repo: &str,
) -> AppResult<()> {
    tx.status(
        format!("creating worktree at {}", worktree_path.display()),
        Some(repo),
    );

    if let Some(parent) = worktree_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut args: Vec<&OsStr> = vec![
        "-C".as_ref(),
        clone_path.as_os_str(),
        "worktree".as_ref(),
        "add".as_ref(),
    ];
    if track_from.is_some() {
        args.push("--track".as_ref());
    }
    args.push("-b".as_ref());
    args.push(branch.as_ref());
    args.push(worktree_path.as_os_str());
    if let Some(start_point) = track_from {
        args.push(start_point.as_ref());
    }

    let status = run_streamed("git", args, None, tx, Some(repo)).await?;

    if !status.success() {
        return Err(AppError::Other(format!(
            "git worktree add {} exited with {:?}",
            worktree_path.display(),
            status.code()
        )));
    }
    Ok(())
}

/// Resolve the name of origin's default branch for the repo at `cwd` (a
/// worktree or a clone — worktrees share the clone's refs). Tries
/// `git symbolic-ref refs/remotes/origin/HEAD` first (yields e.g.
/// `origin/master`, from which we strip the `origin/` prefix). If origin/HEAD
/// isn't set locally, falls back to probing `origin/master` then `origin/main`.
pub async fn origin_default_branch(cwd: &Path) -> AppResult<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
        .output()
        .await
        .map_err(|e| AppError::Other(format!("git symbolic-ref: {e}")))?;
    if output.status.success() {
        let referent = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if let Some(branch) = referent.strip_prefix("origin/") {
            if !branch.is_empty() {
                return Ok(branch.to_string());
            }
        }
    }

    for candidate in ["master", "main"] {
        if show_ref_exists(cwd, &format!("refs/remotes/origin/{candidate}")).await? {
            return Ok(candidate.to_string());
        }
    }

    Err(AppError::Other(
        "could not determine origin's default branch (no origin/HEAD, \
         origin/master, or origin/main)"
            .into(),
    ))
}

/// `git -C <cwd> fetch origin --prune`, streamed. Updates the remote-tracking
/// refs shared by all worktrees of the clone so a subsequent merge sees the
/// latest origin commits.
pub async fn fetch_origin(cwd: &Path, tx: &JobTx, repo: &str) -> AppResult<()> {
    tx.status("fetching origin", Some(repo));
    let args: [&OsStr; 6] = [
        "-C".as_ref(),
        cwd.as_os_str(),
        "fetch".as_ref(),
        "origin".as_ref(),
        "--prune".as_ref(),
        "--progress".as_ref(),
    ];
    let status = run_streamed("git", args, None, tx, Some(repo)).await?;
    if !status.success() {
        return Err(AppError::Other(format!(
            "git fetch origin in {} exited with {:?}",
            cwd.display(),
            status.code()
        )));
    }
    Ok(())
}

/// Merge `origin/<branch>` into the worktree's currently checked-out branch
/// with `git -C <cwd> merge --no-edit origin/<branch>`. On a non-zero exit
/// (conflicts, or a dirty working tree that blocked the merge from starting)
/// run `git merge --abort` so the worktree is left clean, then bubble an error
/// describing what happened. Callers are expected to `fetch_origin` first.
pub async fn merge_origin_branch(
    cwd: &Path,
    branch: &str,
    tx: &JobTx,
    repo: &str,
) -> AppResult<()> {
    tx.status(format!("merging origin/{branch}"), Some(repo));
    let merge_ref = format!("origin/{branch}");
    let args: [&OsStr; 5] = [
        "-C".as_ref(),
        cwd.as_os_str(),
        "merge".as_ref(),
        "--no-edit".as_ref(),
        merge_ref.as_ref(),
    ];
    let status = run_streamed("git", args, None, tx, Some(repo)).await?;
    if !status.success() {
        // Abort so we never leave the worktree mid-merge with conflict
        // markers. If no merge was actually in progress (e.g. a dirty tree
        // blocked it from starting), `merge --abort` errors — harmless here.
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["merge", "--abort"])
            .output()
            .await;
        return Err(AppError::Other(format!(
            "git merge origin/{branch} in {} failed — resolve conflicts or \
             commit/stash local changes, then retry. The worktree was left \
             unchanged.",
            cwd.display()
        )));
    }
    Ok(())
}

/// `git -C <clone_path> show-ref --verify --quiet refs/heads/<branch>`.
/// Returns true if the branch exists locally in the clone. Non-zero exit
/// means the branch doesn't exist — not an error.
pub async fn branch_exists(clone_path: &Path, branch: &str) -> AppResult<bool> {
    show_ref_exists(clone_path, &format!("refs/heads/{branch}")).await
}

/// `git -C <clone_path> show-ref --verify --quiet refs/remotes/<remote>/<branch>`.
/// Returns true if the remote-tracking branch exists in the clone. The clone
/// is expected to be freshly pulled before this is called, so a `true` here
/// means the branch is genuinely present on the remote.
pub async fn remote_branch_exists(
    clone_path: &Path,
    remote: &str,
    branch: &str,
) -> AppResult<bool> {
    show_ref_exists(clone_path, &format!("refs/remotes/{remote}/{branch}")).await
}

async fn show_ref_exists(clone_path: &Path, refspec: &str) -> AppResult<bool> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .arg("show-ref")
        .arg("--verify")
        .arg("--quiet")
        .arg(refspec)
        .output()
        .await
        .map_err(|e| AppError::Other(format!("git show-ref: {e}")))?;
    Ok(output.status.success())
}

/// `git -C <clone_path> worktree prune`. Best-effort: clears stale worktree
/// registrations for directories that no longer exist. Errors are logged
/// but not bubbled. Run before `branch -D` so git won't refuse with "branch
/// in use by prunable worktree".
pub async fn worktree_prune_best_effort(clone_path: &Path, tx: &JobTx, repo: &str) {
    let args: [&OsStr; 4] = [
        "-C".as_ref(),
        clone_path.as_os_str(),
        "worktree".as_ref(),
        "prune".as_ref(),
    ];
    match run_streamed("git", args, None, tx, Some(repo)).await {
        Ok(status) if status.success() => {}
        Ok(status) => tx.status(
            format!("worktree prune exited with {:?}", status.code()),
            Some(repo),
        ),
        Err(e) => tx.status(format!("worktree prune failed: {e}"), Some(repo)),
    }
}

/// `git -C <clone_path> branch -D <branch>`. Best-effort: a non-zero exit
/// (e.g. the branch doesn't exist) is logged but not bubbled. Used as
/// cleanup when a workspace is deleted, so the same branch name can be
/// reused for a new workspace.
pub async fn branch_delete_best_effort(
    clone_path: &Path,
    branch: &str,
    tx: &JobTx,
    repo: &str,
) {
    tx.status(format!("deleting branch {branch}"), Some(repo));
    let args: [&OsStr; 5] = [
        "-C".as_ref(),
        clone_path.as_os_str(),
        "branch".as_ref(),
        "-D".as_ref(),
        branch.as_ref(),
    ];
    match run_streamed("git", args, None, tx, Some(repo)).await {
        Ok(status) if status.success() => {}
        Ok(status) => tx.status(
            format!("branch -D {branch} exited with {:?} (already gone?)", status.code()),
            Some(repo),
        ),
        Err(e) => tx.status(
            format!("branch -D {branch} failed: {e}"),
            Some(repo),
        ),
    }
}

/// `git -C <clone_path> worktree remove <worktree_path>`. Silent variant
/// for the background purger: no `JobTx`, no per-line streaming.
pub async fn worktree_remove_silent(
    clone_path: &Path,
    worktree_path: &Path,
    force: bool,
) -> AppResult<()> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(clone_path)
        .arg("worktree")
        .arg("remove");
    if force {
        cmd.arg("--force");
    }
    cmd.arg(worktree_path);
    let output = cmd
        .output()
        .await
        .map_err(|e| AppError::Other(format!("git worktree remove: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AppError::Other(format!(
            "git worktree remove {} exited with {:?}: {stderr}",
            worktree_path.display(),
            output.status.code()
        )));
    }
    Ok(())
}

/// `git -C <clone_path> worktree prune`. Silent best-effort variant.
pub async fn worktree_prune_best_effort_silent(clone_path: &Path) {
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .arg("worktree")
        .arg("prune")
        .output()
        .await;
}

/// `git -C <clone_path> branch -D <branch>`. Silent best-effort variant.
pub async fn branch_delete_best_effort_silent(clone_path: &Path, branch: &str) {
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .arg("branch")
        .arg("-D")
        .arg(branch)
        .output()
        .await;
}

/// `git -C <clone_path> worktree remove <worktree_path>`. Returns an error if
/// the worktree is dirty (caller can retry with `force`).
pub async fn worktree_remove(
    clone_path: &Path,
    worktree_path: &Path,
    force: bool,
    tx: &JobTx,
    repo: &str,
) -> AppResult<()> {
    tx.status(
        format!("removing worktree {}", worktree_path.display()),
        Some(repo),
    );

    let mut args: Vec<&OsStr> = vec![
        "-C".as_ref(),
        clone_path.as_os_str(),
        "worktree".as_ref(),
        "remove".as_ref(),
    ];
    if force {
        args.push("--force".as_ref());
    }
    args.push(worktree_path.as_os_str());

    let status = run_streamed("git", args, None, tx, Some(repo)).await?;

    if !status.success() {
        return Err(AppError::Other(format!(
            "git worktree remove {} exited with {:?}",
            worktree_path.display(),
            status.code()
        )));
    }
    Ok(())
}
