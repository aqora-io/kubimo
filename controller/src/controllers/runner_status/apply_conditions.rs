use kubimo::k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kubimo::{Runner, RunnerStatus, Workspace, prelude::*};

use super::RunnerStatusReconciler;
use super::conditions::{
    pod_ready_condition, pod_scheduled_condition, pvc_bound_condition, startup_complete,
    upsert_condition, workspace_ready_condition,
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
        let pods = ctx.api_namespaced::<Pod>(namespace);
        let pvcs = ctx.api_namespaced::<PersistentVolumeClaim>(namespace);
        let workspaces = ctx.api_namespaced::<Workspace>(namespace);
        let (pod, pvc, workspace) = futures::try_join!(
            pods.get_opt(name),
            pvcs.get_opt(workspace_name),
            workspaces.get_opt(workspace_name),
        )?;
        let generation = runner.metadata.generation;
        let conditions = status.conditions.get_or_insert_with(Vec::new);
        upsert_condition(
            conditions,
            pvc_bound_condition(workspace_name, pvc.as_ref(), generation),
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
