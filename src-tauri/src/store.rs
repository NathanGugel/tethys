use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, RwLock};
use tracing::{debug, error, info, warn};

use crate::error::AppResult;
use crate::state::{AppState, WorkspaceStatus};

/// The source of truth for Tethys workspace state.
///
/// Writes go through `mutate`, which applies the closure under a write lock
/// and nudges a background flusher. The flusher coalesces bursts of writes
/// (~250ms debounce) into a single atomic temp-file + rename.
pub struct Store {
    state: Arc<RwLock<AppState>>,
    dirty: Arc<Notify>,
    state_path: PathBuf,
    tmp_path: PathBuf,
}

const DEBOUNCE: Duration = Duration::from_millis(250);

impl Store {
    /// Load `state.json` (or initialize an empty state), then start the background flusher.
    pub async fn load(state_path: PathBuf, tmp_path: PathBuf) -> AppResult<Arc<Self>> {
        let mut initial = match tokio::fs::read(&state_path).await {
            Ok(bytes) if !bytes.is_empty() => match serde_json::from_slice::<AppState>(&bytes) {
                Ok(s) => {
                    info!(workspaces = s.workspaces.len(), "loaded state.json");
                    s
                }
                Err(e) => {
                    error!(error = %e, "state.json failed to parse; starting empty");
                    AppState::default()
                }
            },
            Ok(_) => AppState::default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("no state.json yet; starting empty");
                AppState::default()
            }
            Err(e) => return Err(e.into()),
        };

        // A `Creating` entry means the previous run crashed mid-provision;
        // a `CreationFailed` entry means the user never dismissed it before
        // shutdown. Either way the in-memory progress events that drove the
        // log pane are gone, so the row is dead UI — drop it. The boot-time
        // reconciler picks up any worktree directories left on disk.
        let pruned = initial
            .workspaces
            .iter()
            .filter(|w| !matches!(w.status, WorkspaceStatus::Ready))
            .map(|w| w.id.clone())
            .collect::<Vec<_>>();
        if !pruned.is_empty() {
            info!(count = pruned.len(), "pruning non-Ready workspaces from state");
            initial
                .workspaces
                .retain(|w| matches!(w.status, WorkspaceStatus::Ready));
        }

        let store = Arc::new(Self {
            state: Arc::new(RwLock::new(initial)),
            dirty: Arc::new(Notify::new()),
            state_path,
            tmp_path,
        });

        store.clone().spawn_flusher();
        Ok(store)
    }

    /// Read-only access to the state.
    pub async fn read<R, F: FnOnce(&AppState) -> R>(&self, f: F) -> R {
        let guard = self.state.read().await;
        f(&guard)
    }

    /// Apply a mutation under a write lock and schedule a flush.
    pub async fn mutate<R, F>(&self, f: F) -> AppResult<R>
    where
        F: FnOnce(&mut AppState) -> AppResult<R>,
    {
        let result = {
            let mut guard = self.state.write().await;
            f(&mut guard)?
        };
        self.dirty.notify_one();
        Ok(result)
    }

    fn spawn_flusher(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                self.dirty.notified().await;
                tokio::time::sleep(DEBOUNCE).await;

                if let Err(e) = self.flush().await {
                    error!(error = %e, "flush failed");
                }
            }
        });
    }

    async fn flush(&self) -> AppResult<()> {
        let snapshot = {
            let guard = self.state.read().await;
            serde_json::to_vec_pretty(&*guard)?
        };

        if let Some(parent) = self.state_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&self.tmp_path, &snapshot).await?;

        // Best-effort fsync on the temp file before rename.
        match tokio::fs::File::options()
            .write(true)
            .open(&self.tmp_path)
            .await
        {
            Ok(f) => {
                if let Err(e) = f.sync_all().await {
                    warn!(error = %e, "fsync of state.json.tmp failed");
                }
            }
            Err(e) => warn!(error = %e, "reopen of state.json.tmp for fsync failed"),
        }

        tokio::fs::rename(&self.tmp_path, &self.state_path).await?;
        debug!(bytes = snapshot.len(), "flushed state.json");
        Ok(())
    }
}
