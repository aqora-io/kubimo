mod apply_job;
mod apply_pvc;

use std::sync::Arc;

use futures::prelude::*;
use kubimo::KubimoWorkspace;
use kubimo::k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kubimo::kube::runtime::{Controller, controller::Action};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Kubimo(#[from] kubimo::Error),
    #[error(transparent)]
    Shlex(#[from] shlex::QuoteError),
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceReconciler;

#[async_trait::async_trait]
impl Reconciler for WorkspaceReconciler {
    type Resource = KubimoWorkspace;
    type Error = Error;

    async fn apply(
        &self,
        ctx: &Context,
        workspace: &KubimoWorkspace,
    ) -> Result<Action, Self::Error> {
        futures::future::try_join_all([
            self.apply_pvc(ctx, workspace).map_ok(|_| ()).boxed(),
            self.apply_job(ctx, workspace).map_ok(|_| ()).boxed(),
        ])
        .await?;
        Ok(Action::await_change())
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<KubimoWorkspace, ReconcileError<Error>>>,
    ReconcileError<Error>,
> {
    let bmows = ctx.api::<KubimoWorkspace>().kube().clone();
    let pvc = ctx.api::<PersistentVolumeClaim>().kube().clone();
    Ok(Controller::new(bmows, Default::default())
        .owns(pvc, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            WorkspaceReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
