use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::{Runner, RunnerStatus, Workspace, prelude::*};

use super::RunnerStatusReconciler;
use super::conditions::{
    claim_bound_condition, pod_ready_condition, pod_scheduled_condition, slot_bound_condition,
    startup_complete, upsert_condition, workspace_ready_condition,
};
use crate::context::Context;

impl RunnerStatusReconciler {
    /// Updates the runner's startup progress conditions. Returns true once
    /// all of them are True.
    pub(super) async fn apply_startup_conditions(
        &self,
        ctx: &Context,
        runner: &Runner,
        status: &mut RunnerStatus,
    ) -> kubimo::Result<bool> {
        let namespace = runner.require_namespace()?;
        let name = runner.name()?;
        let workspace_name = runner.spec.workspace.as_str();
        // A claimed runner's pod is the warm pod it adopted, which keeps its
        // pool name; only unclaimed runners have a pod named after themselves.
        let claim = runner.status.as_ref().and_then(|s| s.claim.as_ref());
        let pod_name = claim.map(|claim| claim.pod_name.as_str()).unwrap_or(name);
        let pods = ctx.api_namespaced::<Pod>(namespace);
        let workspaces = ctx.api_namespaced::<Workspace>(namespace);
        let (pod, workspace) =
            futures::try_join!(pods.get_opt(pod_name), workspaces.get_opt(workspace_name),)?;
        let generation = runner.metadata.generation;
        let conditions = status.conditions.get_or_insert_with(Vec::new);
        upsert_condition(
            conditions,
            match claim {
                // A claimed pod's containers started long before the claim, so
                // the container-started proxy would lie; the agent's ack is
                // the honest signal.
                Some(_) => claim_bound_condition(pod.as_ref(), generation),
                None => slot_bound_condition(pod.as_ref(), generation),
            },
        );
        upsert_condition(
            conditions,
            workspace_ready_condition(workspace_name, workspace.as_ref(), generation),
        );
        upsert_condition(
            conditions,
            pod_scheduled_condition(pod.as_ref(), generation),
        );
        upsert_condition(conditions, pod_ready_condition(pod.as_ref(), generation));
        Ok(startup_complete(conditions))
    }
}
