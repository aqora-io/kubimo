use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::{Workspace, WorkspaceStatus, prelude::*};

use crate::context::Context;
use crate::controllers::runner_status::conditions::upsert_condition;

use super::WorkspaceReconciler;

/// `reason` of the `Ready=True` condition. Distinct strings were once needed to
/// tell the pooled path from the init-Job one in logs and `kubectl get`, and
/// this one is kept because the platform-facing value must not change.
pub(crate) const POOLED_READY_REASON: &str = "Ready";

impl WorkspaceReconciler {
    pub(crate) async fn apply_status(
        &self,
        ctx: &Context,
        workspace: &Workspace,
    ) -> Result<(), kubimo::Error> {
        if workspace.metadata.deletion_timestamp.is_some() {
            return Ok(());
        }
        let namespace = workspace.require_namespace()?;
        // Nothing to provision ahead of the runner: the slot is created on
        // demand by the node agent when kubelet publishes the runner's inline
        // volume.
        //
        // Ready deliberately does not wait on the archive existing, and could
        // not: the controller has no S3 client, and the credentials for a
        // workspace's bucket live in the Secret `spec.indexer.pod.envFrom`
        // names, which kubelet delivers to the agent as the slot volume's
        // `nodePublishSecretRef` and never to this process. Both components
        // that do hold them
        // check it instead — the client writes and verifies a workspace's seed
        // archive before creating the CR, and the agent records
        // `status.archive.lastSyncedAt` once a slot's content has actually
        // reached S3, which is what tells a workspace that has never been
        // persisted from one that is genuinely empty.
        //
        // Ready therefore keeps meaning "nothing is left to provision".
        // Changing it is not a local decision: runners are not reconciled
        // until their workspace is Ready, and a brand-new workspace has no
        // archive by definition, so gating on one would stop it ever starting
        // its first runner.
        let mut status = build_workspace_status(workspace);
        status.python_runtime = Some(workspace.spec.python_runtime.unwrap_or_default());
        let mut workspace = workspace.clone();
        workspace.status = Some(status);
        ctx.api_namespaced::<Workspace>(namespace)
            .patch_status(&workspace)
            .await?;
        Ok(())
    }
}

fn build_workspace_status(workspace: &Workspace) -> WorkspaceStatus {
    let ready = Condition {
        // upsert_condition keeps an existing condition's timestamp unless
        // `status` actually flips, so `now` only lands on first write.
        last_transition_time: Time(Timestamp::now()),
        observed_generation: workspace.metadata.generation,
        message: "Workspace is ready; its slot is created on demand".into(),
        reason: POOLED_READY_REASON.into(),
        status: "True".into(),
        type_: "Ready".into(),
    };
    let mut conditions = workspace
        .status
        .as_ref()
        .and_then(|status| status.conditions.clone())
        .unwrap_or_default();
    upsert_condition(&mut conditions, ready);
    // upsert_condition only refreshes observedGeneration when status or reason
    // changes, and Ready's never do, so without this it would stay at the
    // generation of the first reconcile through every later spec edit.
    if let Some(ready) = conditions.iter_mut().find(|cond| cond.type_ == "Ready") {
        ready.observed_generation = workspace.metadata.generation;
    }
    // Send only `conditions`. `storage` is owned by the agent, which writes it
    // under the field manager "kubimo-indexer" — the identity of the standalone
    // indexer pod it replaced, kept so its applies stay idempotent — under
    // server-side apply; copying it into this patch would make
    // "kubimo-controller" co-claim those fields and 409-conflict with the
    // agent's writes. Omitting it (storage stays `None`, skipped on serialize)
    // leaves the agent's value untouched on the server.
    WorkspaceStatus {
        conditions: Some(conditions),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spec edit bumps `metadata.generation` but leaves Ready's status and
    /// reason untouched, which is exactly the case `upsert_condition` treats
    /// as "nothing to update" — so the generation must be pinned separately,
    /// without disturbing the transition timestamp.
    #[test]
    fn ready_observed_generation_follows_the_spec_generation() {
        let mut workspace = Workspace::new("test", Default::default());
        workspace.metadata.generation = Some(2);
        workspace.status = Some(WorkspaceStatus {
            conditions: Some(vec![Condition {
                type_: "Ready".to_string(),
                status: "True".to_string(),
                reason: POOLED_READY_REASON.to_string(),
                message: "Workspace is ready; its slot is created on demand".to_string(),
                observed_generation: Some(1),
                last_transition_time: Time(Timestamp::UNIX_EPOCH),
            }]),
            ..Default::default()
        });

        let status = build_workspace_status(&workspace);
        let conditions = status.conditions.unwrap();
        let ready = conditions
            .iter()
            .find(|cond| cond.type_ == "Ready")
            .unwrap();
        assert_eq!(ready.observed_generation, Some(2));
        assert_eq!(ready.status, "True");
        assert_eq!(ready.reason, POOLED_READY_REASON);
        assert_eq!(ready.last_transition_time, Time(Timestamp::UNIX_EPOCH));
    }
}
