mod apply_job;
mod apply_pvc;
mod apply_secret;
mod apply_status;

use std::sync::Arc;

use futures::prelude::*;
use kubimo::Workspace;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, Secret};
use kubimo::kube::runtime::{Controller, controller::Action};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct WorkspaceReconciler;

#[async_trait::async_trait]
impl Reconciler for WorkspaceReconciler {
    type Resource = Workspace;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, workspace: &Workspace) -> Result<Action, Self::Error> {
        futures::future::try_join_all([
            self.apply_pvc(ctx, workspace).map_ok(|_| ()).boxed(),
            self.apply_secret(ctx, workspace).map_ok(|_| ()).boxed(),
            self.apply_job(ctx, workspace).map_ok(|_| ()).boxed(),
            self.apply_status(ctx, workspace).boxed(),
        ])
        .await?;
        Ok(Action::await_change())
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<Workspace, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let bmows = ctx.api_global::<Workspace>().kube().clone();
    let pvc = ctx.api_global::<PersistentVolumeClaim>().kube().clone();
    let jobs = ctx.api_global::<Job>().kube().clone();
    let secrets = ctx.api_global::<Secret>().kube().clone();
    Ok(Controller::new(bmows, Default::default())
        .owns(pvc, Default::default())
        .owns(jobs, Default::default())
        .owns(secrets, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            WorkspaceReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
