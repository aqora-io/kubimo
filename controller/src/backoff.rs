use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};
use kubimo::kube::runtime::controller::Action;
use kubimo::kube::runtime::{utils::Backoff, watcher::DefaultBackoff};
use thiserror::Error;
use tokio::sync::Mutex;
use tower::{Layer, Service};

use crate::context::Context;

#[derive(Debug, Error)]
pub struct BackoffError<E> {
    #[source]
    pub error: E,
    pub backoff: Option<Duration>,
}

impl<E> BackoffError<E> {
    pub fn new(error: E, backoff: Option<Duration>) -> Self {
        Self { error, backoff }
    }
}

impl<E> fmt::Display for BackoffError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(backoff) = self.backoff {
            write!(f, "{} (next wait: {:?})", self.error, backoff)
        } else {
            self.error.fmt(f)
        }
    }
}

pub trait BackoffBuilder {
    type Backoff: Backoff;
    fn build(&self) -> Self::Backoff;
}

pub struct BackoffService<S, B> {
    inner: S,
    backoff: Arc<Mutex<B>>,
}

impl<S, B, R> Service<R> for BackoffService<S, B>
where
    S: Service<R>,
    S::Response: Send + 'static,
    S::Error: Send + 'static,
    S::Future: Send + 'static,
    B: Backoff + 'static,
{
    type Response = S::Response;
    type Error = BackoffError<S::Error>;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner
            .poll_ready(cx)
            .map_err(|e| BackoffError::new(e, None))
    }
    fn call(&mut self, r: R) -> Self::Future {
        let ret = self.inner.call(r);
        let backoff = self.backoff.clone();
        async move {
            match ret.await {
                Ok(res) => {
                    backoff.lock().await.reset();
                    Ok(res)
                }
                Err(err) => Err(BackoffError::new(err, backoff.lock().await.next())),
            }
        }
        .boxed()
    }
}

#[derive(Clone, Default)]
pub struct BackoffLayer<B> {
    backoff_builder: B,
}

impl<S, B> Layer<S> for BackoffLayer<B>
where
    B: BackoffBuilder,
{
    type Service = BackoffService<S, B::Backoff>;

    fn layer(&self, inner: S) -> Self::Service {
        BackoffService {
            inner,
            backoff: Arc::new(Mutex::new(self.backoff_builder.build())),
        }
    }
}

#[derive(Clone, Default)]
pub struct DefaultBackoffBuilder;

impl BackoffBuilder for DefaultBackoffBuilder {
    type Backoff = DefaultBackoff;

    fn build(&self) -> Self::Backoff {
        DefaultBackoff::default()
    }
}

pub type DefaultBackoffLayer = BackoffLayer<DefaultBackoffBuilder>;

pub fn default_error_policy<R, E>(
    _object: Arc<R>,
    error: &BackoffError<E>,
    _ctx: Arc<Context>,
) -> Action {
    if let Some(backoff) = error.backoff {
        Action::requeue(backoff)
    } else {
        Action::await_change()
    }
}
