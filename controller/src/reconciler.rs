use std::fmt::Debug;
use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt};
use kubimo::k8s_openapi::NamespaceResourceScope;
use kubimo::kube::{Resource, runtime::controller::Action};
use serde::{Serialize, de::DeserializeOwned};
use tower::{Service, ServiceBuilder};

use crate::backoff::{BackoffError, DefaultBackoffLayer};
use crate::context::Context;
use crate::service::{Finalizer, FinalizerError, reconcile};
use crate::tracing::TraceLayer;

#[async_trait::async_trait]
pub trait Reconciler {
    type Resource;
    type Error;
    async fn apply(&self, ctx: &Context, resource: &Self::Resource) -> Result<Action, Self::Error>;
    async fn cleanup(
        &self,
        _ctx: &Context,
        _resource: &Self::Resource,
    ) -> Result<Action, Self::Error> {
        Ok(Action::await_change())
    }
}

pub type ReconcileError<E> = BackoffError<FinalizerError<E>>;
type ReconcileFuture<E> = BoxFuture<'static, Result<Action, ReconcileError<E>>>;
type ReconcileFn<R, E> = Box<dyn FnMut(Arc<R>, Arc<Context>) -> ReconcileFuture<E> + Send>;

#[allow(clippy::type_complexity)]
pub trait ReconcilerExt: Reconciler {
    fn service(
        self,
        name: impl ToString,
    ) -> impl Service<
        (Arc<Self::Resource>, Arc<Context>),
        Response = Action,
        Error = ReconcileError<Self::Error>,
        Future = ReconcileFuture<Self::Error>,
    > + Send
    + Sync
    + 'static
    where
        Self: Sized + Send + Sync + 'static,
        Self::Resource: Resource<Scope = NamespaceResourceScope>
            + Serialize
            + DeserializeOwned
            + Clone
            + Debug
            + Send
            + Sync
            + 'static,
        <Self::Resource as Resource>::DynamicType: Default,
        Self::Error: std::error::Error + Send + 'static,
    {
        ServiceBuilder::new()
            .layer(TraceLayer)
            .layer(DefaultBackoffLayer::default())
            .service(Finalizer::new(name, self))
    }

    fn reconcile(
        self,
        name: impl ToString,
    ) -> BoxFuture<
        'static,
        Result<ReconcileFn<Self::Resource, Self::Error>, ReconcileError<Self::Error>>,
    >
    where
        Self: Sized + Send + Sync + 'static,
        Self::Resource: Resource<Scope = NamespaceResourceScope>
            + Serialize
            + DeserializeOwned
            + Clone
            + Debug
            + Send
            + Sync
            + 'static,
        <Self::Resource as Resource>::DynamicType: Default,
        Self::Error: std::error::Error + Send + 'static,
    {
        reconcile(self.service(name)).boxed()
    }
}

impl<T> ReconcilerExt for T where T: Reconciler {}
