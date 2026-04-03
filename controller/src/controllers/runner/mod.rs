mod apply_ingress;
mod apply_owner_reference;
mod apply_pod;
mod apply_service;

use std::sync::Arc;
use std::{collections::BTreeMap, time::Duration};

use futures::prelude::*;
use kubimo::kube::runtime::{Controller, controller::Action};
use kubimo::{KubimoLabel, Runner, prelude::*};
use kubimo::{
    Workspace,
    k8s_openapi::api::{
        core::v1::{Pod, Service},
        networking::v1::Ingress,
    },
};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct RunnerReconciler;

impl RunnerReconciler {
    fn pod_labels(&self, runner: &Runner) -> kubimo::Result<BTreeMap<String, String>> {
        Ok([(
            KubimoLabel::borrow("name").to_string(),
            runner.name()?.to_string(),
        )]
        .into_iter()
        .collect())
    }
}

#[async_trait::async_trait]
impl Reconciler for RunnerReconciler {
    type Resource = Runner;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, runner: &Runner) -> Result<Action, Self::Error> {
        let namespace = runner.require_namespace()?;
        let workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get_opt(&runner.spec.workspace)
            .await?;
        match workspace {
            // Workspace does not exist, weird but we'll wait
            None => return Ok(Action::requeue(Duration::from_secs(10))),
            // Workspace is not ready yet
            Some(workspace) if !is_workspace_ready(&workspace) => {
                return Ok(Action::requeue(Duration::from_secs(5)));
            }
            // Workspace is ready
            Some(_) => {}
        }

        futures::future::try_join_all([
            self.apply_owner_reference(ctx, runner).boxed(),
            self.apply_pod(ctx, runner).map_ok(|_| ()).boxed(),
            self.apply_service(ctx, runner).map_ok(|_| ()).boxed(),
            self.apply_ingress(ctx, runner).map_ok(|_| ()).boxed(),
        ])
        .await?;

        Ok(Action::await_change())
    }
}

fn is_workspace_ready(workspace: &Workspace) -> bool {
    workspace.status.as_ref().is_some_and(|status| {
        status
            .conditions
            .as_ref()
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
    })
}

pub fn controller(ctx: &Context) -> Controller<Runner> {
    let bmors = ctx.api_global::<Runner>().kube().clone();
    let pods = ctx.api_global::<Pod>().kube().clone();
    let svcs = ctx.api_global::<Service>().kube().clone();
    let ings = ctx.api_global::<Ingress>().kube().clone();
    Controller::new(bmors, Default::default())
        .owns(pods, Default::default())
        .owns(svcs, Default::default())
        .owns(ings, Default::default())
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<Runner, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    Ok(controller(&ctx).graceful_shutdown_on(shutdown_signal).run(
        RunnerReconciler.reconcile("controller").await?,
        default_error_policy,
        ctx,
    ))
}
