//! Tracks workspace IDs currently being created.
//!
//! `create_workspace` only persists to state.json on full success, so while
//! it's running there are directories under `worktree_root` that have no
//! matching workspace in state — which is exactly what the reconciler flags
//! as "orphaned". We register the id here for the duration of the create so
//! `reconcile::scan` can skip it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct InProgressWorkspaces {
    inner: Arc<Mutex<HashSet<String>>>,
}

impl InProgressWorkspaces {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `id` as in-progress and return a guard. Dropping the guard
    /// (normal return, `?`, panic, or task cancellation) removes it.
    pub fn insert(&self, id: String) -> InProgressGuard {
        self.inner.lock().unwrap().insert(id.clone());
        InProgressGuard {
            inner: self.inner.clone(),
            id,
        }
    }

    pub fn snapshot(&self) -> HashSet<String> {
        self.inner.lock().unwrap().clone()
    }
}

pub struct InProgressGuard {
    inner: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for InProgressGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.inner.lock() {
            set.remove(&self.id);
        }
    }
}
