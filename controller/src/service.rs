use std::fmt::Debug;
use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt};
use kubimo::k8s_openapi::NamespaceResourceScope;
use kubimo::kube::{
    Resource,
    runtime::{
        controller::Action,
        finalizer::{Event, finalizer},
    },
};
use kubimo::prelude::*;
use serde::{Serialize, de::DeserializeOwned};
use tower::{Service, ServiceExt};

use crate::context::Context;
use crate::reconciler::Reconciler;

pub use kubimo::kube::runtime::finalizer::Error as FinalizerError;

pub struct Finalizer<T> {
    name: String,
    reconciler: Arc<T>,
}

impl<T> Finalizer<T> {
    pub fn new(name: impl ToString, reconciler: T) -> Self {
        Self {
            name: name.to_string(),
            reconciler: Arc::new(reconciler),
        }
    }
}

impl<T, R> Service<(Arc<R>, Arc<Context>)> for Finalizer<T>
where
    T: Reconciler<Resource = R> + Send + Sync + 'static,
    R: Resource<Scope = NamespaceResourceScope>
        + Serialize
        + DeserializeOwned
        + Clone
        + Debug
        + Send
        + Sync
        + 'static,
    <R as Resource>::DynamicType: Default,
    R: Send + Sync,
    T::Error: std::error::Error + Send + 'static,
{
    type Response = Action;
    type Error = FinalizerError<T::Error>;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn call(&mut self, (resource, ctx): (Arc<R>, Arc<Context>)) -> Self::Future {
        let finalizer_name = self.name.clone();
        let reconciler = self.reconciler.clone();
        async move {
            let namespace = resource.require_namespace().map_err(|err| {
                FinalizerError::AddFinalizer(kubimo::kube::Error::Service(err.into()))
            })?;
            finalizer(
                ctx.api_namespaced::<R>(namespace).kube(),
                &finalizer_name,
                resource,
                |event| async move {
                    match event {
                        Event::Apply(r) => reconciler.apply(&ctx, &r).await,
                        Event::Cleanup(r) => reconciler.cleanup(&ctx, &r).await,
                    }
                },
            )
            .await
        }
        .boxed()
    }
}

pub async fn reconcile<S, T>(
    mut service: S,
) -> Result<Box<dyn FnMut(Arc<T>, Arc<Context>) -> S::Future + Send>, S::Error>
where
    S: Service<(Arc<T>, Arc<Context>)> + Send + 'static,
{
    service.ready().await?;
    Ok(Box::new(move |resource: Arc<T>, ctx: Arc<Context>| {
        service.call((resource, ctx))
    }))
}
