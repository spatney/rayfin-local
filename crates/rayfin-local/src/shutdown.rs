//! Stopping `serve`: the stop signals, and the hooks that stop what `serve` started.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

type HookFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type Hook = Box<dyn FnOnce() -> HookFuture + Send>;

/// What to stop on shutdown (child processes, watchers), stopped newest first.
#[derive(Default)]
pub struct StopHooks {
    hooks: Vec<(String, Hook)>,
}

impl StopHooks {
    /// Registers `hook` (for example `move || async move { child.kill(grace).await; }`).
    #[allow(
        dead_code,
        reason = "the functions and frontend integrations register their processes here"
    )]
    pub fn add<F, Fut>(&mut self, label: impl Into<String>, hook: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.hooks.push((
            label.into(),
            Box::new(move || Box::pin(hook()) as HookFuture),
        ));
    }

    /// Runs every hook, newest first, giving each up to `timeout`.
    pub async fn run(self, timeout: Duration) {
        for (label, hook) in self.hooks.into_iter().rev() {
            tracing::debug!("stopping {label}");
            if tokio::time::timeout(timeout, hook()).await.is_err() {
                tracing::warn!("{label} did not stop within {} seconds", timeout.as_secs());
            }
        }
    }
}

/// Completes on the next Ctrl+C (also Ctrl+Break and closing the console on Windows, and
/// SIGTERM on Unix).
pub async fn signal() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let mut ctrl_c = windows::ctrl_c()?;
        let mut ctrl_break = windows::ctrl_break()?;
        let mut ctrl_close = windows::ctrl_close()?;
        tokio::select! {
            _ = ctrl_c.recv() => {}
            _ = ctrl_break.recv() => {}
            _ = ctrl_close.recv() => {}
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[tokio::test]
    async fn hooks_run_newest_first_and_a_stuck_one_does_not_block_the_rest() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut hooks = StopHooks::default();
        for label in ["backend", "stuck", "vite"] {
            let order = Arc::clone(&order);
            hooks.add(label, move || async move {
                if label == "stuck" {
                    std::future::pending::<()>().await;
                }
                order.lock().unwrap().push(label);
            });
        }
        hooks.run(Duration::from_millis(50)).await;
        assert_eq!(*order.lock().unwrap(), ["vite", "backend"]);
    }
}
