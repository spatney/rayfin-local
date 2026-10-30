//! Stopping `serve`: the stop signals, the end of stdin (`--json`), and the hooks that stop what
//! `serve` started.

use std::future::Future;
use std::io::{self, Read};
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::oneshot;

type HookFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type Hook = Box<dyn FnOnce() -> HookFuture + Send>;

/// What to stop on shutdown (child processes, watchers), stopped newest first.
#[derive(Default)]
pub struct StopHooks {
    hooks: Vec<(String, Hook)>,
}

impl StopHooks {
    /// Registers `hook` (for example `move || async move { child.kill(grace).await; }`).
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

/// What asked `serve` to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopRequest {
    /// Ctrl+C (or another signal of [`signal`]).
    Signal,
    /// stdin ended: the host closed it, or ended.
    StdinClosed,
}

/// Listens for what asks `serve` to stop: the signals of [`signal`] and the end of an input
/// (stdin, for hosts).
pub struct StopListener {
    eof: oneshot::Receiver<()>,
}

impl StopListener {
    /// Starts reading `input` (on a thread of its own) to see it end.
    pub fn with_input(input: impl Read + Send + 'static) -> Self {
        Self {
            eof: watch_eof(input),
        }
    }

    /// The next stop request.
    pub async fn requested(&mut self) -> io::Result<StopRequest> {
        let eof = async {
            // A reader that could not run never ends.
            if (&mut self.eof).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            signal = signal() => signal.map(|()| StopRequest::Signal),
            () = eof => Ok(StopRequest::StdinClosed),
        }
    }
}

/// Reads `input` until it ends or fails, on a thread of its own (a blocking read can't be
/// cancelled); the receiver completes then. What is read is ignored.
pub fn watch_eof(mut input: impl Read + Send + 'static) -> oneshot::Receiver<()> {
    let (ended, receiver) = oneshot::channel();
    let reader = move || {
        let mut buffer = [0; 512];
        loop {
            match input.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = ended.send(());
    };
    if let Err(err) = std::thread::Builder::new()
        .name("stdin".into())
        .spawn(reader)
    {
        tracing::warn!("could not watch stdin, so closing it does not stop rayfin-local: {err}");
    }
    receiver
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

    /// A pipe whose writer is a channel: reads block until data comes or the sender is dropped.
    struct Pipe(std::sync::mpsc::Receiver<Vec<u8>>);

    impl Read for Pipe {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            match self.0.recv() {
                Ok(bytes) => {
                    let n = bytes.len().min(buffer.len());
                    buffer[..n].copy_from_slice(&bytes[..n]);
                    Ok(n)
                }
                Err(_) => Ok(0),
            }
        }
    }

    #[tokio::test]
    async fn the_end_of_stdin_asks_to_stop() {
        let (writer, reader) = std::sync::mpsc::channel();
        let mut listener = StopListener::with_input(Pipe(reader));
        writer.send(b"anything a host writes".to_vec()).unwrap();
        let early = tokio::time::timeout(Duration::from_millis(200), listener.requested()).await;
        assert!(early.is_err(), "stopped while stdin was open");

        drop(writer);
        let request = tokio::time::timeout(Duration::from_secs(5), listener.requested())
            .await
            .expect("no stop request after stdin ended")
            .unwrap();
        assert_eq!(request, StopRequest::StdinClosed);
    }
}
