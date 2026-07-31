use kubimo::k8s_crd_snapshot_storage::VolumeSnapshot;
use kubimo::k8s_openapi::api::batch::v1::Job;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::{Workspace, WorkspaceMode, WorkspacePythonRuntime, WorkspaceStatus, prelude::*};

use crate::context::Context;

use super::WorkspaceReconciler;

/// `reason` of the `Ready=False` condition written when a Workspace is refused
/// provisioning because it does not fit its budget.
pub(crate) const BUDGET_EXCEEDED_REASON: &str = "BudgetExceeded";

/// `reason` of the `Ready=True` condition written for a `Pooled` Workspace.
/// Distinct from `JobComplete` so the two paths are distinguishable in logs and
/// `kubectl get`, though the platform only reads `status`.
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
        let name = workspace.name()?;
        let mode = workspace.effective_mode(ctx.config.default_workspace_mode);
        if mode == WorkspaceMode::Pooled {
            // Nothing to provision ahead of the runner: there is no PVC to bind
            // and no init Job to complete. The slot is created on demand by the
            // node agent when kubelet publishes the runner's inline volume.
            //
            // Ready deliberately does not wait on the archive existing, and
            // could not: the controller has no S3 client, and the credentials
            // for a workspace's bucket are in its own `spec.indexer.pod` env,
            // handed to pods and never to this process. Both components that
            // do hold them check it instead — the client writes and verifies a
            // workspace's seed archive before creating the CR, and the agent
            // records `status.archive.lastSyncedAt` once a slot's content has
            // actually reached S3, which is what tells a workspace that has
            // never been persisted from one that is genuinely empty.
            //
            // Ready therefore keeps meaning "nothing is left to provision".
            // Changing it is not a local decision: runners are not reconciled
            // until their workspace is Ready, and a brand-new pooled workspace
            // has no archive by definition, so gating on one would stop it ever
            // starting its first runner.
            let mut status = build_workspace_status(workspace, None, StatusKind::PooledReady, mode);
            status.python_runtime = Some(get_workspace_python_runtime(ctx, workspace).await?);
            let mut workspace = workspace.clone();
            workspace.status = Some(status);
            ctx.api_namespaced::<Workspace>(namespace)
                .patch_status(&workspace)
                .await?;
            return Ok(());
        }

        let mut status =
            if let Some(job) = ctx.api_namespaced::<Job>(namespace).get_opt(name).await? {
                build_workspace_status(
                    workspace,
                    job_last_transition_time(&job),
                    StatusKind::from_job(&job),
                    mode,
                )
            } else {
                let status = if workspace.spec.clone_workspace_name.is_some() {
                    let snap_is_ready = ctx
                        .api_namespaced::<VolumeSnapshot>(namespace)
                        .get_opt(name)
                        .await?
                        .and_then(|snap| snap.status)
                        .and_then(|st| st.ready_to_use)
                        .unwrap_or_default();
                    if snap_is_ready {
                        StatusKind::JobComplete
                    } else {
                        StatusKind::JobNotComplete
                    }
                } else {
                    // Not Complete unless its job was created
                    StatusKind::JobNotComplete
                };
                build_workspace_status(workspace, None, status, mode)
            };

        status.python_runtime = Some(get_workspace_python_runtime(ctx, workspace).await?);

        let mut workspace = workspace.clone();
        workspace.status = Some(status);

        ctx.api_namespaced::<Workspace>(namespace)
            .patch_status(&workspace)
            .await?;
        Ok(())
    }

    /// Mark the workspace not-Ready because provisioning was refused by a budget.
    pub(crate) async fn apply_budget_status(
        &self,
        ctx: &Context,
        workspace: &Workspace,
        reason: &str,
    ) -> Result<(), kubimo::Error> {
        if workspace.metadata.deletion_timestamp.is_some() {
            return Ok(());
        }
        let namespace = workspace.require_namespace()?;
        let mode = workspace.effective_mode(ctx.config.default_workspace_mode);
        let mut status = build_workspace_status(
            workspace,
            None,
            StatusKind::BudgetExceeded(reason.to_owned()),
            mode,
        );
        status.python_runtime = Some(get_workspace_python_runtime(ctx, workspace).await?);
        let mut workspace = workspace.clone();
        workspace.status = Some(status);
        ctx.api_namespaced::<Workspace>(namespace)
            .patch_status(&workspace)
            .await?;
        Ok(())
    }
}

#[allow(clippy::enum_variant_names)] // redudant variants but useful if we add non-job related
enum StatusKind {
    JobFailed,
    JobNotComplete,
    JobComplete,
    BudgetExceeded(String),
    PooledReady,
}

impl StatusKind {
    fn from_job(job: &Job) -> Self {
        let job_conditions = job
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_deref())
            .unwrap_or_default();
        let job_complete = job_conditions.iter().find(|cond| cond.type_ == "Complete");
        let job_failed = job_conditions.iter().find(|cond| cond.type_ == "Failed");
        if job_failed.is_some_and(|cond| cond.status == "True") {
            Self::JobFailed
        } else if job_complete.is_none_or(|cond| cond.status == "False") {
            Self::JobNotComplete
        } else {
            Self::JobComplete
        }
    }
}

fn job_last_transition_time(job: &Job) -> Option<Time> {
    job.status
        .as_ref()
        .and_then(|status| status.conditions.as_deref())
        .unwrap_or_default()
        .iter()
        .filter_map(|cond| cond.last_transition_time.as_ref())
        .max()
        .cloned()
}

fn build_workspace_status(
    workspace: &Workspace,
    last_transition_time: Option<Time>,
    kind: StatusKind,
    mode: WorkspaceMode,
) -> WorkspaceStatus {
    let last_transition_time = last_transition_time
        .or(workspace.metadata.creation_timestamp.clone())
        .unwrap_or_else(|| Time(Timestamp::now()));
    let observed_generation = workspace.metadata.generation;
    let ready = match kind {
        StatusKind::JobFailed => Condition {
            last_transition_time,
            observed_generation,
            message: "Job failed".into(),
            reason: "JobFailed".into(),
            status: "False".into(),
            type_: "Ready".into(),
        },
        StatusKind::JobNotComplete => Condition {
            last_transition_time,
            observed_generation,
            message: "Job not complete".into(),
            reason: "JobNotComplete".into(),
            status: "False".into(),
            type_: "Ready".into(),
        },
        StatusKind::JobComplete => Condition {
            last_transition_time,
            observed_generation,
            message: "Job complete".into(),
            reason: "JobComplete".into(),
            status: "True".into(),
            type_: "Ready".into(),
        },
        StatusKind::BudgetExceeded(message) => Condition {
            last_transition_time,
            observed_generation,
            message,
            reason: BUDGET_EXCEEDED_REASON.into(),
            status: "False".into(),
            type_: "Ready".into(),
        },
        StatusKind::PooledReady => Condition {
            last_transition_time,
            observed_generation,
            message: "Workspace is ready; its slot is created on demand".into(),
            reason: POOLED_READY_REASON.into(),
            status: "True".into(),
            type_: "Ready".into(),
        },
    };
    let mut conditions = workspace
        .status
        .as_ref()
        .and_then(|status| status.conditions.clone())
        .unwrap_or_default();
    if let Some(current_ready) = conditions.iter_mut().find(|cond| cond.type_ == "Ready") {
        *current_ready = ready;
    } else {
        conditions.push(ready);
    }
    // Send only `conditions` and `mode`. `storage` is owned by the indexer's field
    // manager ("kubimo-indexer") under server-side apply; copying it into this patch
    // would make "kubimo-controller" co-claim those fields and 409-conflict with the
    // indexer's writes. Omitting it (storage stays `None`, skipped on serialize)
    // leaves the indexer's value untouched on the server.
    //
    // `mode` is materialized here so the workspace stops depending on the
    // operator's default. `effective_mode` already prefers `status.mode`, so
    // re-writing it every reconcile is idempotent and self-pinning: once set,
    // changing `KUBIMO__DEFAULT_WORKSPACE_MODE` can never re-mode this object
    // and orphan its PVC.
    WorkspaceStatus {
        conditions: Some(conditions),
        mode: Some(mode),
        ..Default::default()
    }
}

pub(crate) async fn get_workspace_python_runtime(
    ctx: &Context,
    workspace: &Workspace,
) -> Result<WorkspacePythonRuntime, kubimo::Error> {
    if let Some(clone_workspace_name) = workspace.spec.clone_workspace_name.as_ref() {
        let namespace = workspace.require_namespace()?;
        let clone_workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get(clone_workspace_name)
            .await?;
        let clone_status = clone_workspace.status.as_ref().ok_or_else(|| {
            kubimo::Error::Custom(format!("Workspace has no status: {clone_workspace_name:?}"))
        })?;
        let python_runtime = clone_status.python_runtime.ok_or_else(|| {
            kubimo::Error::Custom(format!(
                "Workspace has no python runtime: {clone_workspace_name:?}"
            ))
        })?;
        Ok(python_runtime)
    } else {
        Ok(workspace.spec.python_runtime.unwrap_or_default())
    }
}
