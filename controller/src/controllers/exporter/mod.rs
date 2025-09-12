mod apply_job;
mod apply_owner_reference;

use std::sync::Arc;

use futures::prelude::*;
use kubimo::KubimoExporter;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::kube::runtime::{Controller, controller::Action};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct ExporterReconciler;

#[async_trait::async_trait]
impl Reconciler for ExporterReconciler {
    type Resource = KubimoExporter;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, exporter: &KubimoExporter) -> Result<Action, Self::Error> {
        futures::future::try_join_all([
            self.apply_owner_reference(ctx, exporter).boxed(),
            self.apply_job(ctx, exporter).map_ok(|_| ()).boxed(),
        ])
        .await?;
        Ok(Action::await_change())
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<KubimoExporter, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    let bmows = ctx.api::<KubimoExporter>().kube().clone();
    let jobs = ctx.api::<Job>().kube().clone();
    Ok(Controller::new(bmows, Default::default())
        .owns(jobs, Default::default())
        .graceful_shutdown_on(shutdown_signal)
        .run(
            ExporterReconciler.reconcile("controller").await?,
            default_error_policy,
            ctx,
        ))
}
