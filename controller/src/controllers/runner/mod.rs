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
use crate::controllers::workspace_affinity;
use crate::controllers::workspace_python_runtime::get_workspace_python_runtime;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Debug, Clone, Copy)]
struct RunnerReconciler;

impl RunnerReconciler {
    fn pod_labels(&self, runner: &Runner) -> kubimo::Result<BTreeMap<String, String>> {
        let mut labels: BTreeMap<String, String> = [(
            KubimoLabel::borrow("name").to_string(),
            runner.name()?.to_string(),
        )]
        .into_iter()
        .collect();
        labels.extend(workspace_affinity::workspace_label_map(
            &runner.spec.workspace,
        ));
        Ok(labels)
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
        let workspace = match workspace {
            // Workspace does not exist
            None => {
                return Err(kubimo::Error::Custom(format!(
                    "Runner bound to workspace that does not exist: {workspace:?}",
                    workspace = runner.spec.workspace
                )));
            }
            // Workspace is not ready yet
            Some(workspace) if !is_workspace_ready(&workspace) => {
                return Ok(Action::requeue(Duration::from_secs(5)));
            }
            // Workspace is ready
            Some(workspace) => workspace,
        };

        let python_runtime = get_workspace_python_runtime(&workspace)?;

        let applied = futures::future::try_join_all([
            self.apply_owner_reference(ctx, runner)
                .map_ok(|_| false)
                .boxed(),
            self.apply_pod(ctx, runner, &workspace, python_runtime)
                .map_ok(|applied| matches!(applied, apply_pod::PodApply::Replaced))
                .boxed(),
            self.apply_service(ctx, runner).map_ok(|_| false).boxed(),
            self.apply_ingress(ctx, runner).map_ok(|_| false).boxed(),
        ])
        .await;

        // Recreating the pod while the previous one is still Terminating is a 409, and a
        // transient one: the name frees up as soon as it finishes. Left to propagate, it
        // would log a reconcile error every time the agent's drain or stale-mount sweep
        // deletes a runner, and on every manual pod delete — which is exactly when
        // someone is reading these logs.
        //
        // Everything else propagates, 422s included. A 422 that apply_pod did *not*
        // confirm as the known runtime-class drift is an ordinary validation failure —
        // a Runner whose resources put requests over limits, say — and never converges;
        // requeuing those on a fixed 2s timer would spin forever instead of letting the
        // controller's backoff space them out.
        let replaced = match applied {
            Ok(outcomes) => outcomes.into_iter().any(|replaced| replaced),
            Err(err) if is_already_exists(&err) => {
                return Ok(Action::requeue(Duration::from_secs(2)));
            }
            Err(err) => return Err(err),
        };

        if replaced {
            // apply_pod deleted a pod whose immutable runtimeClassName had drifted.
            // Nothing has recreated it, and its deletion is not a change this
            // controller can wait on, so come back once the old one has finished
            // terminating and the name is free again.
            tracing::info!(
                runner = runner.name()?,
                "replaced a drifted pod; requeuing to recreate it"
            );
            return Ok(Action::requeue(Duration::from_secs(2)));
        }

        Ok(Action::await_change())
    }
}

/// A 409 from the API server, i.e. the object still exists under this name.
///
/// For a pod being recreated after a delete this is transient — the name frees up once
/// the previous one finishes terminating — so it should requeue rather than surface as a
/// reconcile failure.
pub(crate) fn is_already_exists(err: &kubimo::Error) -> bool {
    matches!(
        err,
        kubimo::Error::Kube(kubimo::kube::Error::Api(status)) if status.code == 409
    )
}

/// A 422 from the API server, and nothing more specific than that.
///
/// An apply that tries to change an immutable field (e.g. a pod's
/// `runtimeClassName`) returns one, but so does every other validation failure,
/// so this is only ever a cheap pre-filter: the caller confirms the actual drift
/// against the live pod before deleting anything, and a 422 it cannot account
/// for is returned to the caller unchanged.
pub(crate) fn is_invalid_request(err: &kubimo::Error) -> bool {
    matches!(
        err,
        kubimo::Error::Kube(kubimo::kube::Error::Api(status)) if status.code == 422
    )
}

pub(crate) fn is_workspace_ready(workspace: &Workspace) -> bool {
    workspace.status.as_ref().is_some_and(|status| {
        let ready_condition = status
            .conditions
            .as_ref()
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
        let has_python_runtime = status.python_runtime.is_some();
        ready_condition && has_python_runtime
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_422_reads_as_an_invalid_request() {
        let invalid = kubimo::Error::Kube(kubimo::kube::Error::Api(
            kubimo::kube::core::Status::failure("pod updates may not change fields", "Invalid")
                .with_code(422)
                .boxed(),
        ));
        assert!(is_invalid_request(&invalid));

        let conflict = kubimo::Error::Kube(kubimo::kube::Error::Api(
            kubimo::kube::core::Status::failure("still terminating", "Conflict")
                .with_code(409)
                .boxed(),
        ));
        assert!(!is_invalid_request(&conflict));
    }
}
