use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use gpui::{App, Global};
use std::{future::Future, sync::Arc};

/// Every database driver Quarry uses (sqlx, tiberius, the mongodb crate, redis, the ClickHouse
/// HTTP client) is written against tokio, and several of them panic outright when polled
/// without a tokio reactor in scope. gpui runs on smol, so there is nowhere in the app to await
/// them directly.
///
/// Rather than teach each driver a different executor, Quarry owns one multi-threaded tokio
/// runtime and treats it as a service: work is handed over with `spawn`, and the result comes
/// back through a oneshot that a gpui task can await like any other future. The runtime lives
/// in a `Global` so a single pool is shared by every connection and every query view.
pub struct QuarryRuntime {
    runtime: Arc<tokio::runtime::Runtime>,
}

struct GlobalQuarryRuntime(QuarryRuntime);

impl Global for GlobalQuarryRuntime {}

impl QuarryRuntime {
    fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            // Connections spend nearly all of their time blocked on a socket, so this pool is
            // sized for concurrency rather than for CPU work. Keeping it small also keeps an
            // idle Quarry from holding a thread per core for nothing.
            .worker_threads(2)
            .thread_name("quarry")
            .build()
            .context("building the Quarry tokio runtime")?;
        Ok(Self {
            runtime: Arc::new(runtime),
        })
    }

    /// Installs the runtime. Called from `quarry::init`; failing here leaves Quarry unavailable
    /// rather than taking the whole app down, since a missing runtime only costs this feature.
    pub fn init(cx: &mut App) {
        match Self::new() {
            Ok(runtime) => cx.set_global(GlobalQuarryRuntime(runtime)),
            Err(error) => {
                log::error!("Quarry is disabled: {error:#}");
            }
        }
    }

    pub fn is_available(cx: &App) -> bool {
        cx.has_global::<GlobalQuarryRuntime>()
    }

    pub fn global(cx: &App) -> Option<Self> {
        cx.try_global::<GlobalQuarryRuntime>().map(|global| Self {
            runtime: global.0.runtime.clone(),
        })
    }

    /// Runs `future` on the tokio runtime and resolves on the caller's executor.
    ///
    /// The returned future is plain `async`, so a gpui task can await it without ever entering
    /// tokio. If the spawned work panics the oneshot is dropped, which surfaces as an error
    /// here instead of unwinding into gpui.
    pub fn spawn<F, T>(&self, future: F) -> impl Future<Output = Result<T>> + Send + 'static
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.runtime.spawn(async move {
            // A send failure just means the caller stopped caring (view closed, query
            // cancelled); the work is already done and there is nothing to report.
            let _ = tx.send(future.await);
        });
        async move {
            rx.await
                .context("the Quarry runtime dropped the task before it reported a result")?
        }
    }
}

impl Clone for QuarryRuntime {
    fn clone(&self) -> Self {
        Self {
            runtime: self.runtime.clone(),
        }
    }
}
