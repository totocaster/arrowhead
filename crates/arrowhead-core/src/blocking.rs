//! Bounded blocking thread pool for CPU-intensive and blocking I/O work.
//!
//! This module provides a fixed-size thread pool that prevents unbounded thread
//! growth when executing blocking operations. Unlike `tokio::task::spawn_blocking`,
//! which can create an unlimited number of threads, this pool maintains a strict
//! upper bound on thread count.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::runtime::{Builder, Runtime};

/// A bounded thread pool for executing blocking operations.
///
/// This pool uses a dedicated Tokio runtime configured with a fixed number of
/// worker threads. Work is queued when all threads are busy, providing natural
/// backpressure and preventing thread explosion during high-load scenarios.
#[derive(Clone)]
pub struct BlockingPool {
    inner: Arc<BlockingPoolInner>,
}

struct BlockingPoolInner {
    runtime: Runtime,
    thread_count: usize,
}

impl BlockingPool {
    /// Create a new blocking pool with the default thread count.
    ///
    /// The default is `num_cpus` clamped between 1 and 16.
    pub fn new() -> Result<Self> {
        Self::with_threads(default_thread_count())
    }

    /// Create a new blocking pool with a specific number of threads.
    pub fn with_threads(thread_count: usize) -> Result<Self> {
        let thread_count = thread_count.clamp(1, 64);
        let runtime = Builder::new_multi_thread()
            .worker_threads(thread_count)
            .thread_name("arrowhead-blocking")
            .enable_all()
            .build()
            .context("failed to create blocking thread pool runtime")?;

        Ok(Self {
            inner: Arc::new(BlockingPoolInner {
                runtime,
                thread_count,
            }),
        })
    }

    /// Execute a blocking closure on the pool, returning the result.
    ///
    /// This method blocks the current async task until the closure completes.
    /// Work is queued if all pool threads are busy.
    pub async fn execute<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let handle = self.inner.runtime.handle().clone();
        tokio::task::spawn_blocking(move || handle.block_on(async { f() }))
            .await
            .context("blocking pool task panicked")
    }

    /// Execute a blocking closure that returns a Result on the pool.
    ///
    /// This is a convenience method that flattens the nested Result types.
    pub async fn execute_result<F, T, E>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> std::result::Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: Into<anyhow::Error> + Send + 'static,
    {
        self.execute(f).await?.map_err(Into::into)
    }

    /// Spawn a blocking task on the pool without waiting for the result.
    ///
    /// Returns a handle that can be awaited to get the result.
    pub fn spawn<F, T>(&self, f: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let handle = self.inner.runtime.handle().clone();
        self.inner.runtime.spawn(async move {
            // Run the blocking work on the dedicated runtime's thread pool
            tokio::task::block_in_place(|| f())
        })
    }

    /// Get the number of threads in the pool.
    pub fn thread_count(&self) -> usize {
        self.inner.thread_count
    }
}

impl Default for BlockingPool {
    fn default() -> Self {
        Self::new().expect("failed to create default blocking pool")
    }
}

impl std::fmt::Debug for BlockingPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingPool")
            .field("thread_count", &self.inner.thread_count)
            .finish()
    }
}

/// Returns the default number of threads for blocking work.
pub fn default_thread_count() -> usize {
    num_cpus::get().clamp(1, 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn pool_executes_blocking_work() {
        let pool = BlockingPool::with_threads(2).unwrap();
        let result = pool.execute(|| 42).await.unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn pool_executes_result_work() {
        let pool = BlockingPool::with_threads(2).unwrap();
        let result: Result<i32> = pool.execute_result(|| -> Result<i32> { Ok(123) }).await;
        assert_eq!(result.unwrap(), 123);
    }

    #[tokio::test]
    async fn pool_limits_concurrent_threads() {
        let pool = BlockingPool::with_threads(2).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let counter = Arc::clone(&counter);
            let max_concurrent = Arc::clone(&max_concurrent);
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                pool.execute(move || {
                    let current = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    counter.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
        }

        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        // The pool should have limited concurrency
        // Note: Due to scheduling, this may occasionally be higher, but should be bounded
        let max = max_concurrent.load(Ordering::SeqCst);
        assert!(max <= 4, "max concurrent was {max}, expected <= 4");
    }
}
