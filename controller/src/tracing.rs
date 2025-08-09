use std::fmt::Debug;
use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt};
use kubimo::prelude::*;
use tower::{Layer, Service};
use tracing::Level;

use crate::context::Context;

pub struct TraceService<S> {
    inner: S,
}

impl<S, T> Service<(Arc<T>, Arc<Context>)> for TraceService<S>
where
    S: Service<(Arc<T>, Arc<Context>)> + Send,
    T: Resource + Debug + Send + Sync,
    <T as Resource>::DynamicType: Default,
    S::Future: Send + 'static,
    S::Error: Debug,
    S::Response: Debug,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, req: (Arc<T>, Arc<Context>)) -> Self::Future {
        let span = tracing::span!(Level::DEBUG, "reconciler", resource = ?req.0);
        let fut = {
            let _guard = span.enter();
            self.inner.call(req)
        };
        async move {
            let _guard = span.enter();
            match fut.await {
                Ok(ret) => {
                    tracing::info!("Reconciled {ret:?}",);
                    Ok(ret)
                }
                Err(err) => {
                    tracing::error!("Error {err:?}",);
                    Err(err)
                }
            }
        }
        .boxed()
    }
}

pub struct TraceLayer;

impl<S> Layer<S> for TraceLayer {
    type Service = TraceService<S>;
    fn layer(&self, inner: S) -> TraceService<S> {
        TraceService { inner }
    }
}
