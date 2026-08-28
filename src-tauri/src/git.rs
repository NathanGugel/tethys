use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

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
    let (status, _) = run_inner(program, args, cwd, tx, repo, false).await?;
    Ok(status)
}

async fn run_inner<I, S>(
    program: &str,
    args: I,
    cwd: Option<&Path>,
    tx: &JobTx,
    repo: Option<&str>,
    capture: bool,
) -> AppResult<(std::process::ExitStatus, Vec<String>)>
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
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = capture.then(|| captured.clone());
    let stderr_task = tokio::spawn(async move {
        drain_lines_capturing(stderr, &tx_err, LogStream::Stderr, repo_err.as_deref(), sink).await;
    });

    let status = child.wait().await?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let stderr_lines = std::mem::take(&mut *captured.lock().unwrap());
    Ok((status, stderr_lines))
}

/// Run a child process, streaming output as `run_streamed` does, but also
/// returning its stderr lines. Used where a non-zero exit needs to explain
/// *why* — an exit code alone sends the reader to the job log to find the
/// `fatal:` that the error should have carried in the first place.
pub async fn run_streamed_capturing_stderr<I, S>(
    program: &str,
    args: I,
    cwd: Option<&Path>,
    tx: &JobTx,
    repo: Option<&str>,
) -> AppResult<(std::process::ExitStatus, Vec<String>)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_inner(program, args, cwd, tx, repo, true).await
}

/// The first `fatal:`/`error:` line git produced, or its last line — whichever
/// is likeliest to say something a person can act on.
pub fn explain(stderr_lines: &[String]) -> Option<String> {
    stderr_lines
        .iter()
        .find(|l| {
            let t = l.trim_start();
            t.starts_with("fatal:") || t.starts_with("error:")
        })
        .or_else(|| stderr_lines.iter().rfind(|l| !l.trim().is_empty()))
        .map(|l| l.trim().to_string())
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
    reader: R,
    tx: &JobTx,
    stream: LogStream,
    repo: Option<&str>,
) {
    drain_lines_capturing(reader, tx, stream, repo, None).await
}

/// As `drain_lines`, but also appends each emitted line to `sink` when given.
async fn drain_lines_capturing<R: AsyncRead + Unpin>(
    mut reader: R,
    tx: &JobTx,
    stream: LogStream,
    repo: Option<&str>,
    sink: Option<Arc<Mutex<Vec<String>>>>,
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
                            let text = String::from_utf8_lossy(&line).into_owned();
                            if let Some(sink) = &sink {
                                sink.lock().unwrap().push(text.clone());
                            }
                            tx.log(stream, text, repo);
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
        let text = String::from_utf8_lossy(&line).into_owned();
        if let Some(sink) = &sink {
            sink.lock().unwrap().push(text.clone());
        }
        tx.log(stream, text, repo);
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

/// How `worktree_add` should resolve the branch it checks out.
pub enum WorktreeBranch<'a> {
    /// Create a fresh branch off the clone's current HEAD (`-b <branch>`).
    NewFromHead,
    /// Create a fresh local branch tracking the given start point, e.g.
    /// `origin/<branch>` (`--track -b <branch> <path> <start>`). Lands the
    /// worktree on the remote's commit with upstream wired up in one step.
    TrackRemote(&'a str),
    /// Check out a branch that already exists locally (`<path> <branch>`).
    /// Git refuses if that branch is already checked out in another worktree,
    /// which is the guard against two workspaces sharing a branch.
    ExistingLocal,
}

/// Adds a worktree at `worktree_path` checked out on `branch`, resolved
/// according to `source`.
pub async fn worktree_add(
    clone_path: &Path,
    worktree_path: &Path,
    branch: &str,
    source: WorktreeBranch<'_>,
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
    match source {
        WorktreeBranch::NewFromHead => {
            args.push("-b".as_ref());
            args.push(branch.as_ref());
            args.push(worktree_path.as_os_str());
        }
        WorktreeBranch::TrackRemote(start_point) => {
            args.push("--track".as_ref());
            args.push("-b".as_ref());
            args.push(branch.as_ref());
            args.push(worktree_path.as_os_str());
            args.push(start_point.as_ref());
        }
        WorktreeBranch::ExistingLocal => {
            args.push(worktree_path.as_os_str());
            args.push(branch.as_ref());
        }
    }

    let (status, stderr) = run_streamed_capturing_stderr("git", args, None, tx, Some(repo)).await?;

    if !status.success() {
        // Git's own `fatal:` says something actionable ("already used by
        // worktree at ..."); the exit code alone sends the reader to the job
        // log to find out what happened.
        return Err(AppError::Other(match explain(&stderr) {
            Some(reason) => format!(
                "git worktree add {} failed: {reason}",
                worktree_path.display()
            ),
            None => format!(
                "git worktree add {} exited with {:?}",
                worktree_path.display(),
                status.code()
            ),
        }));
    }
    Ok(())
}

/// Detach a worktree's HEAD at the commit it is already on.
///
/// This is how a branch is freed for another worktree to take. Detaching
/// rather than switching branches is deliberate: the files stay at the exact
/// same commit, so nothing running against that worktree sees its contents
/// change — it simply stops owning the branch name. Local modifications are
/// carried across untouched.
pub async fn detach_worktree(worktree_path: &Path) -> AppResult<()> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["checkout", "--detach"])
        .output()
        .await
        .map_err(|e| AppError::Other(format!("git checkout --detach: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<String> = stderr.lines().map(str::to_string).collect();
        return Err(AppError::Other(match explain(&lines) {
            Some(reason) => format!(
                "could not detach {}: {reason}",
                worktree_path.display()
            ),
            None => format!("could not detach {}", worktree_path.display()),
        }));
    }
    Ok(())
}

/// Path of the worktree that currently has `branch` checked out, if any.
///
/// Git allows a branch in only one worktree at a time, and every worktree for
/// a repo comes off the same managed clone — so this is how a caller finds out
/// *who* is holding a branch it wants, rather than only that someone is.
pub async fn worktree_holding_branch(
    clone_path: &Path,
    branch: &str,
) -> AppResult<Option<PathBuf>> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(clone_path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .await
        .map_err(|e| AppError::Other(format!("git worktree list: {e}")))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(parse_worktree_holding_branch(
        &String::from_utf8_lossy(&out.stdout),
        branch,
    ))
}

/// Scan `git worktree list --porcelain` for the entry whose `branch` line
/// matches. Records are blank-line separated and lead with `worktree <path>`.
fn parse_worktree_holding_branch(porcelain: &str, branch: &str) -> Option<PathBuf> {
    let wanted = format!("refs/heads/{branch}");
    let mut current: Option<PathBuf> = None;
    for line in porcelain.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path.trim()));
        } else if let Some(found) = line.strip_prefix("branch ") {
            if found.trim() == wanted {
                return current;
            }
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    /// A `JobTx` whose receiver is dropped. Every send in this module is
    /// fire-and-forget (`let _ = send`), so the closed channel is inert.
    fn noop_tx() -> JobTx {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        JobTx(tx)
    }

    fn git_ok(cwd: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .expect("git must run");
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    /// A repo with one commit on `main`, usable as a clone source.
    fn init_repo_with_commit(path: &Path) {
        git_ok(path, &["init", "--initial-branch=main"]);
        git_ok(path, &["config", "user.email", "test@example.com"]);
        git_ok(path, &["config", "user.name", "Test"]);
        std::fs::write(path.join("README.md"), "hello\n").expect("write README");
        git_ok(path, &["add", "."]);
        git_ok(path, &["commit", "-m", "initial"]);
    }

    fn clone_repo(origin: &Path, dest: &Path) {
        let status = StdCommand::new("git")
            .arg("clone")
            .arg(origin)
            .arg(dest)
            .status()
            .expect("git clone must run");
        assert!(status.success(), "git clone failed");
    }

    fn current_branch(path: &Path) -> String {
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("git rev-parse must run");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn worktree_add_checks_out_existing_local_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo_with_commit(&origin);

        let clone_path = tmp.path().join("clone");
        clone_repo(&origin, &clone_path);
        // A branch that already exists locally — e.g. a side branch an agent
        // created and pushed, which the user now wants its own worktree for.
        git_ok(&clone_path, &["branch", "feature"]);
        assert!(branch_exists(&clone_path, "feature").await.unwrap());

        let worktree_path = tmp.path().join("wt");
        worktree_add(
            &clone_path,
            &worktree_path,
            "feature",
            WorktreeBranch::ExistingLocal,
            &noop_tx(),
            "repo",
        )
        .await
        .unwrap();

        assert_eq!(current_branch(&worktree_path), "feature");
    }

    #[tokio::test]
    async fn worktree_add_existing_local_rejects_branch_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo_with_commit(&origin);

        let clone_path = tmp.path().join("clone");
        clone_repo(&origin, &clone_path);
        git_ok(&clone_path, &["branch", "feature"]);

        let first = tmp.path().join("wt1");
        worktree_add(
            &clone_path,
            &first,
            "feature",
            WorktreeBranch::ExistingLocal,
            &noop_tx(),
            "repo",
        )
        .await
        .unwrap();

        // The "another workspace already has this branch checked out" case.
        // Git must refuse rather than produce two worktrees on one branch.
        let second = tmp.path().join("wt2");
        let result = worktree_add(
            &clone_path,
            &second,
            "feature",
            WorktreeBranch::ExistingLocal,
            &noop_tx(),
            "repo",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn worktree_add_new_from_head_creates_the_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo_with_commit(&origin);

        let clone_path = tmp.path().join("clone");
        clone_repo(&origin, &clone_path);
        assert!(!branch_exists(&clone_path, "fresh").await.unwrap());

        let worktree_path = tmp.path().join("wt");
        worktree_add(
            &clone_path,
            &worktree_path,
            "fresh",
            WorktreeBranch::NewFromHead,
            &noop_tx(),
            "repo",
        )
        .await
        .unwrap();

        assert_eq!(current_branch(&worktree_path), "fresh");
        assert!(branch_exists(&clone_path, "fresh").await.unwrap());
    }

    /// The whole point of detaching: the holder keeps its files at the same
    /// commit, gives up the branch name, and a second worktree can then take
    /// it — which is what "give this PR its own workspace" relies on.
    #[tokio::test]
    async fn detaching_frees_the_branch_for_another_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        init_repo_with_commit(&origin);

        let clone_path = tmp.path().join("clone");
        clone_repo(&origin, &clone_path);
        git_ok(&clone_path, &["branch", "feature"]);

        let holder = tmp.path().join("holder");
        worktree_add(
            &clone_path,
            &holder,
            "feature",
            WorktreeBranch::ExistingLocal,
            &noop_tx(),
            "repo",
        )
        .await
        .unwrap();
        let commit_before = current_commit(&holder);

        // A second worktree can't have it yet.
        let taker = tmp.path().join("taker");
        assert!(worktree_add(
            &clone_path,
            &taker,
            "feature",
            WorktreeBranch::ExistingLocal,
            &noop_tx(),
            "repo",
        )
        .await
        .is_err());

        detach_worktree(&holder).await.unwrap();

        // Holder kept its contents, just not the branch.
        assert_eq!(current_commit(&holder), commit_before);
        assert_eq!(current_branch(&holder), "HEAD");
        assert_eq!(
            parse_worktree_holding_branch(
                &String::from_utf8_lossy(
                    &std::process::Command::new("git")
                        .arg("-C")
                        .arg(&clone_path)
                        .args(["worktree", "list", "--porcelain"])
                        .output()
                        .unwrap()
                        .stdout
                ),
                "feature"
            ),
            None
        );

        // And now the move can complete.
        worktree_add(
            &clone_path,
            &taker,
            "feature",
            WorktreeBranch::ExistingLocal,
            &noop_tx(),
            "repo",
        )
        .await
        .unwrap();
        assert_eq!(current_branch(&taker), "feature");
    }

    fn current_commit(path: &Path) -> String {
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse must run");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

#[cfg(test)]
mod holding_tests {
    use super::{explain, parse_worktree_holding_branch};
    use std::path::PathBuf;

    const PORCELAIN: &str = "\
worktree /data/repos/frontend
HEAD 5586c250fc
branch refs/heads/master

worktree /wt/nathan-nl-8883/frontend
HEAD 7ff47a5916
branch refs/heads/nathan/bulk-clone-append-only-copy

worktree /wt/detached/frontend
HEAD abc123
detached
";

    #[test]
    fn finds_the_worktree_holding_a_branch() {
        assert_eq!(
            parse_worktree_holding_branch(PORCELAIN, "nathan/bulk-clone-append-only-copy"),
            Some(PathBuf::from("/wt/nathan-nl-8883/frontend"))
        );
    }

    #[test]
    fn returns_none_for_a_branch_no_worktree_holds() {
        assert_eq!(parse_worktree_holding_branch(PORCELAIN, "nathan/other"), None);
    }

    /// A detached worktree holds no branch, so it must never be reported as
    /// the holder of the branch it happens to sit on.
    #[test]
    fn a_detached_worktree_is_not_a_holder() {
        assert_eq!(parse_worktree_holding_branch(PORCELAIN, "abc123"), None);
    }

    #[test]
    fn explain_prefers_the_fatal_line() {
        let lines = vec![
            "Preparing worktree (checking out 'x')".to_string(),
            "fatal: 'x' is already used by worktree at '/wt/a'".to_string(),
        ];
        assert_eq!(
            explain(&lines).as_deref(),
            Some("fatal: 'x' is already used by worktree at '/wt/a'")
        );
    }

    #[test]
    fn explain_falls_back_to_the_last_nonempty_line() {
        let lines = vec!["something odd".to_string(), "   ".to_string()];
        assert_eq!(explain(&lines).as_deref(), Some("something odd"));
    }

    #[test]
    fn explain_of_nothing_is_none() {
        assert_eq!(explain(&[]), None);
    }
}
