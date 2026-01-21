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
    pub fn new(debounce: Duration, poll: Duration) -> notify::Result<Self> {
        let notify = Arc::new(Notify::new());
        let cloned_notify = notify.clone();
        let (tx, mut rx) = mpsc::channel::<Event>(1000);
        let debouncer = tokio::spawn(async move {
            while rx.recv().await.is_some() {
                let sleep = tokio::time::sleep(debounce);
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
                            sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
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
