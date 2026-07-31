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
        match workspace {
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
            Some(_) => {}
        }

        let applied = futures::future::try_join_all([
            self.apply_owner_reference(ctx, runner).boxed(),
            self.apply_pod(ctx, runner).map_ok(|_| ()).boxed(),
            self.apply_service(ctx, runner).map_ok(|_| ()).boxed(),
            self.apply_ingress(ctx, runner).map_ok(|_| ()).boxed(),
        ])
        .await;

        // Recreating the pod while the previous one is still Terminating is a 409, and a
        // transient one: the name frees up as soon as it finishes. Left to propagate, it
        // would log a reconcile error every time the agent's drain or stale-mount sweep
        // deletes a runner, and on every manual pod delete — which is exactly when
        // someone is reading these logs.
        //
        // A 422 means the apply hit an immutable pod field; when apply_pod confirmed the
        // known runtime-class drift it has already deleted the pod, and this requeue is
        // what recreates it once the old one finishes terminating.
        if let Err(err) = applied {
            if is_already_exists(&err) || is_immutable_conflict(&err) {
                return Ok(Action::requeue(Duration::from_secs(2)));
            }
            return Err(err);
        }

        Ok(Action::await_change())
    }
}

/// A 409 from the API server, i.e. the object still exists under this name.
///
/// For a pod being recreated after a delete this is transient — the name frees up once
/// the previous one finishes terminating — so it should requeue rather than surface as a
/// reconcile failure.
fn is_already_exists(err: &kubimo::Error) -> bool {
    matches!(
        err,
        kubimo::Error::Kube(kubimo::kube::Error::Api(status)) if status.code == 409
    )
}

/// A 422 from the API server — what an apply that tries to change an immutable
/// field (e.g. a pod's `runtimeClassName`) returns, and one that never resolves
/// on its own retry: the field stays wrong forever unless the object is
/// replaced.
///
/// A 422 is also what any other validation failure returns, so this alone must
/// not justify anything destructive: apply_pod confirms the actual drift
/// against the live pod before deleting.
fn is_immutable_conflict(err: &kubimo::Error) -> bool {
    matches!(
        err,
        kubimo::Error::Kube(kubimo::kube::Error::Api(status)) if status.code == 422
    )
}

pub(crate) fn is_workspace_ready(workspace: &Workspace) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_422_reads_as_an_immutable_field_conflict() {
        let invalid = kubimo::Error::Kube(kubimo::kube::Error::Api(
            kubimo::kube::core::Status::failure("pod updates may not change fields", "Invalid")
                .with_code(422)
                .boxed(),
        ));
        assert!(is_immutable_conflict(&invalid));

        let conflict = kubimo::Error::Kube(kubimo::kube::Error::Api(
            kubimo::kube::core::Status::failure("still terminating", "Conflict")
                .with_code(409)
                .boxed(),
        ));
        assert!(!is_immutable_conflict(&conflict));
    }
}
