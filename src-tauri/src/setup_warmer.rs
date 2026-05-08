use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::unbounded_channel;
use tracing::{info, warn};

use crate::error::AppResult;
use crate::git;
use crate::job::{JobEvent, JobTx, LogStream};
use crate::paths::Paths;
use crate::registry::{RegistryLoad, Repo};
use crate::setup;

/// How often to refresh each repo's base clone. Each tick runs a fast-forward
/// pull and the configured setup script in `<data_dir>/repos/<key>` so that
/// `node_modules` there stays close to current — `provision_repo_worktree`
/// then APFS-clonefiles it into each new worktree.
const WARM_INTERVAL: Duration = Duration::from_secs(3 * 3600);

/// Delay before the first tick so the warmer doesn't fight the github poller,
/// purger, and hook listener for resources during boot.
const INITIAL_DELAY: Duration = Duration::from_secs(30);

pub struct SetupWarmer {
    paths: Paths,
    registry: Arc<RegistryLoad>,
}

impl SetupWarmer {
    pub fn new(paths: Paths, registry: Arc<RegistryLoad>) -> Self {
        Self { paths, registry }
    }

    /// Long-running loop. Spawn with `tokio::spawn(warmer.run())`.
    pub async fn run(self: Arc<Self>) {
        tokio::time::sleep(INITIAL_DELAY).await;
        loop {
            self.tick().await;
            tokio::time::sleep(WARM_INTERVAL).await;
        }
    }

    async fn tick(&self) {
        let repos: Vec<Repo> = match self.registry.require() {
            Ok(reg) => reg
                .repos
                .iter()
                .filter(|r| {
                    r.default_setup_script
                        .as_ref()
                        .is_some_and(|s| !s.trim().is_empty())
                })
                .cloned()
                .collect(),
            Err(e) => {
                warn!(error = %e, "setup_warmer: registry unavailable; skipping tick");
                return;
            }
        };

        if repos.is_empty() {
            return;
        }

        info!(n = repos.len(), "setup_warmer: tick");
        for repo in repos {
            if let Err(e) = warm_repo(&self.paths, &repo).await {
                warn!(repo = %repo.key, error = %e, "setup_warmer: warm failed");
            }
        }
    }
}

async fn warm_repo(paths: &Paths, repo: &Repo) -> AppResult<()> {
    let clone_path = paths.repo_clone_path(&repo.key);
    let tx = tracing_job_tx(&repo.key);

    git::ensure_clone(&clone_path, &repo.remote_url, &tx, &repo.key).await?;
    git::pull_clone(&clone_path, &tx, &repo.key).await?;

    let Some(script) = repo
        .default_setup_script
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(());
    };

    info!(repo = %repo.key, "setup_warmer: running setup in base clone");
    setup::run_setup_script(
        script,
        &clone_path,
        repo.setup_timeout_secs,
        &tx,
        &repo.key,
    )
    .await?;
    info!(repo = %repo.key, "setup_warmer: setup completed");
    Ok(())
}

/// Build a `JobTx` whose events are forwarded to `tracing` instead of a
/// frontend channel — lets the background warmer reuse `setup::run_setup_script`
/// (and the `git::*` helpers) without piping output to the UI.
fn tracing_job_tx(repo: &str) -> JobTx {
    let (tx, mut rx) = unbounded_channel::<JobEvent>();
    let repo = repo.to_string();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                JobEvent::Status { message, .. } => {
                    info!(repo = %repo, "warmer: {message}");
                }
                JobEvent::Log { stream, line, .. } => match stream {
                    LogStream::Stdout => info!(repo = %repo, "warmer: {line}"),
                    LogStream::Stderr => warn!(repo = %repo, "warmer: {line}"),
                },
                JobEvent::Success | JobEvent::Failed { .. } => {}
            }
        }
    });
    JobTx(tx)
}
