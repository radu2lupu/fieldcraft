//! Read-only SQLite connections for the query path.
//!
//! `AppState::repo` is one connection behind one mutex; every write goes
//! through it. Queries used to take that mutex too, so concurrent agents
//! waited on each other (and on the embedding call made under it). In WAL
//! mode readers do not block each other or the writer, so each query borrows
//! its own read-only connection from this pool instead.

use std::path::PathBuf;
use std::sync::Mutex;

use fieldcraft_core::TrError;
use fieldcraft_storage::KnowledgeRepository;

/// Idle connections kept for reuse; more are opened on demand under load.
const MAX_IDLE: usize = 16;

pub struct ReaderPool {
    path: PathBuf,
    idle: Mutex<Vec<KnowledgeRepository>>,
}

impl ReaderPool {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            idle: Mutex::new(Vec::new()),
        }
    }

    /// Run `f` on a pooled read-only connection. Blocking: call from
    /// `spawn_blocking`, never from an async task.
    pub fn with<R>(&self, f: impl FnOnce(&KnowledgeRepository) -> R) -> Result<R, TrError> {
        let pooled = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let repo = match pooled {
            Some(repo) => repo,
            None => KnowledgeRepository::open_reader(&self.path)?,
        };
        let out = f(&repo);
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() < MAX_IDLE {
            idle.push(repo);
        }
        Ok(out)
    }
}
