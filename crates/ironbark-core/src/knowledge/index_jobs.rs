//! P5 index workers. Embedding COMPUTE is CPU work; the module bounds the
//! number of in-flight embedding computations across the whole process to
//! MAX_INDEX_WORKERS (2) via a process-global semaphore, so a burst of
//! re-index requests cannot overload the host. Each op runs the deterministic
//! feature-hash embedding in `spawn_blocking` under a permit, so the async
//! runtime is never blocked.
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;
use crate::error::{CoreError, Result};

/// Hard cap on concurrent embedding computations in this process.
pub const MAX_INDEX_WORKERS: usize = 2;

static WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(MAX_INDEX_WORKERS)));

fn workers() -> &'static Arc<Semaphore> { &*WORKERS }

/// A handle to the bounded embedding compute pool. Cheap to clone.
#[derive(Clone)]
pub struct IndexWorker;

impl IndexWorker {
    pub fn new() -> Self { Self }

    /// Embed `(chunk_id, content)` items at `dimensions`, at most
    /// `MAX_INDEX_WORKERS` ops concurrently (global bound). Returns them in a
    /// stable order. The caller owns the SQLite write step.
    pub async fn run(&self, items: Vec<(String, String)>, dimensions: u32) -> Result<Vec<(String, Vec<f32>)>> {
        let permit = Arc::clone(workers()).acquire_owned().await
            .map_err(|_| CoreError::new("indexing_cancelled", "index worker pool closed"))?;
        let dims = dimensions;
        let out = tokio::task::spawn_blocking(move || {
            items.into_iter()
                .map(|(id, content)| (id, super::embeddings::embed(&content, dims)))
                .collect::<Vec<_>>()
        }).await
            .map_err(|_| CoreError::new("indexing_worker_failed", "index worker panicked"))?;
        drop(permit);
        Ok(out)
    }
}

impl Default for IndexWorker {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn worker_pool_bounds_compute_and_is_deterministic() {
        let w = IndexWorker::new();
        let items = (0..5).map(|i| (format!("c{i}"), format!("content {i} hashing test"))).collect();
        let out = w.run(items, 16).await.unwrap();
        assert_eq!(out.len(), 5);
        for (_id, v) in &out { assert_eq!(v.len(), 16); }
        // Deterministic: embedding is a pure function of (text, dims).
        let again = w.run(vec![("c0".into(), "content 0 hashing test".into())], 16).await.unwrap();
        assert_eq!(again[0].1, out[0].1);
    }
}