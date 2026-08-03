use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use thiserror::Error;
use tokio::{
    signal::ctrl_c,
    sync::{Notify, mpsc},
    task::JoinHandle,
};

pub struct Watcher {
    paths: BTreeSet<PathBuf>,
    inner: RecommendedWatcher,
    notify: Arc<Notify>,
    debouncer: JoinHandle<()>,
    poll: Duration,
    ctrl_c: BoxFuture<'static, std::io::Result<()>>,
}

#[derive(Debug, Error)]
pub enum WaitError {
    #[error("Watcher closed")]
    Closed,
    #[error("Ctrl-C received")]
    CtrlC,
    #[error("Ctrl-C error: {0}")]
    CtrlCError(std::io::Error),
}

impl Watcher {
    /// `max_wait` bounds how long a burst of events can defer a sync.
    ///
    /// Without it the trailing debounce can starve indefinitely: every new
    /// event pushes the deadline out, so a directory that never goes quiet —
    /// a training loop writing checkpoints, a build in a loop — is never
    /// indexed at all. That is exactly when the data is worth keeping, so the
    /// wait is capped rather than left open-ended.
    pub fn new(debounce: Duration, max_wait: Duration, poll: Duration) -> notify::Result<Self> {
        let notify = Arc::new(Notify::new());
        let cloned_notify = notify.clone();
        let (tx, mut rx) = mpsc::channel::<Event>(1000);
        let debouncer = tokio::spawn(async move {
            while rx.recv().await.is_some() {
                // Fixed at the first event of the burst, so resets can push the
                // wake-up later but never past this.
                let deadline = tokio::time::Instant::now() + max_wait;
                let sleep = tokio::time::sleep(debounce.min(max_wait));
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => {
                            cloned_notify.notify_one();
                            break;
                        }
                        maybe = rx.recv() => {
                            if maybe.is_none() {
                                return;
                            }
                            let next = tokio::time::Instant::now() + debounce;
                            sleep.as_mut().reset(next.min(deadline));
                        }
                    }
                }
            }
        });
        let inner = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
            Ok(event) => {
                if (event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove())
                    && let Err(err) = tx.try_send(event)
                {
                    tracing::error!("Watcher notify error: {err}");
                }
            }
            Err(err) => {
                tracing::error!("Watcher error: {err}");
            }
        })?;
        Ok(Self {
            paths: Default::default(),
            inner,
            notify,
            debouncer,
            poll,
            ctrl_c: Box::pin(ctrl_c()),
        })
    }

    pub fn watch(&mut self, paths: BTreeSet<PathBuf>) -> notify::Result<()> {
        let mut paths_mut = self.inner.paths_mut();
        for path in &self.paths {
            if !paths.contains(path) {
                paths_mut.remove(path)?;
            }
        }
        for path in &paths {
            if !self.paths.contains(path) {
                paths_mut.add(path, RecursiveMode::NonRecursive)?;
            }
        }
        paths_mut.commit()?;
        self.paths = paths;
        Ok(())
    }

    pub async fn wait(&mut self) -> Result<(), WaitError> {
        tokio::select! {
            _ = self.notify.notified() => {
                Ok(())
            },
            _ = tokio::time::sleep(self.poll) => {
                Ok(())
            },
            _ = &mut self.debouncer => {
                Err(WaitError::Closed)
            },
            res = &mut self.ctrl_c => {
                match res {
                    Ok(()) => Err(WaitError::CtrlC),
                    Err(err) => Err(WaitError::CtrlCError(err)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The starvation guard, exercised against the real `Watcher`.
    ///
    /// Events arrive faster than the debounce and never stop. Before the
    /// ceiling existed each one reset the timer, so `wait()` never returned and
    /// a continuously-written workspace was never indexed — precisely when its
    /// data is worth keeping. Short durations keep the test around a second.
    #[tokio::test]
    async fn a_continuous_event_stream_still_fires_within_max_wait() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("churn");
        std::fs::write(&file, b"0").unwrap();

        let mut watcher = Watcher::new(
            Duration::from_millis(200),
            Duration::from_millis(600),
            Duration::from_secs(30),
        )
        .unwrap();
        watcher
            .watch(vec![file.clone()].into_iter().collect())
            .unwrap();

        // Rewrite every 50ms: always sooner than the 200ms debounce, so without
        // a ceiling the deadline would be pushed out forever.
        let churn = tokio::spawn(async move {
            for i in 0..200 {
                let _ = std::fs::write(&file, format!("{i}"));
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let fired = tokio::time::timeout(Duration::from_secs(5), watcher.wait()).await;
        churn.abort();
        // Both layers matter: a `wait()` that returns because the debouncer task
        // ended returns immediately, so checking only the timeout would pass on
        // exactly the failure this test exists to catch.
        fired
            .expect("debouncer starved: never fired despite the max-wait ceiling")
            .expect("watcher closed instead of firing");
    }
}
