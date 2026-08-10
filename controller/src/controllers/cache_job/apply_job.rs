use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{Container, PodSpec, PodTemplateSpec, VolumeMount};
use kubimo::kube::api::ObjectMeta;
use kubimo::{CacheJob, Workspace, WorkspaceMode, WorkspacePythonRuntime, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::controllers::indexer;
use crate::controllers::slot_volume;
use crate::controllers::workspace_affinity;
use crate::controllers::workspace_python_runtime::get_workspace_python_runtime;
use crate::resources::Resources;

use super::CacheJobReconciler;

impl CacheJobReconciler {
    fn cache_container(
        &self,
        ctx: &Context,
        cache_job: &CacheJob,
        python_runtime: WorkspacePythonRuntime,
    ) -> Container {
        let workspace_name = cache_job.spec.workspace.clone();
        let mut command = cmd!["bash", "/setup/start.sh"];
        if let Some(log_level) = cache_job.spec.log_level.as_ref() {
            command.extend(cmd!["--log-level", log_level]);
        }
        command.push("cache".into());
        Container {
            name: "cache".into(),
            image: Some(ctx.config.marimo_image(python_runtime).to_string()),
            resources: Resources::default()
                .cpu(cache_job.spec.cpu.clone())
                .memory(cache_job.spec.memory.clone())
                .into(),
            volume_mounts: Some(vec![VolumeMount {
                mount_path: indexer::MOUNT_DIR.into(),
                name: workspace_name,
                ..Default::default()
            }]),
            env: cache_job.spec.env.clone(),
            env_from: cache_job.spec.env_from.clone(),
            command: Some(command),
            ..Default::default()
        }
    }

    fn indexer_container(
        &self,
        ctx: &Context,
        workspace: &Workspace,
        python_runtime: WorkspacePythonRuntime,
    ) -> Result<Container, kubimo::Error> {
        Ok(Container {
            name: "indexer".to_string(),
            image: Some(ctx.config.marimo_image(python_runtime).to_string()),
            command: Some(cmd!["/app/indexer"]),
            args: Some(indexer::upload_args(workspace, false)?),
            env: indexer::env(workspace),
            env_from: indexer::env_from(workspace),
            volume_mounts: Some(vec![VolumeMount {
                mount_path: indexer::MOUNT_DIR.to_string(),
                name: workspace.name()?.to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        })
    }

    pub(crate) async fn apply_job(
        &self,
        ctx: &Context,
        cache_job: &CacheJob,
    ) -> Result<Job, kubimo::Error> {
        let cache_job_name = cache_job.name()?;
        let namespace = cache_job.require_namespace()?;

        if let Some(job) = ctx
            .api_namespaced::<Job>(namespace)
            .get_opt(cache_job_name)
            .await?
        {
            return Ok(job);
        }

        let workspace_name = &cache_job.spec.workspace;
        let workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get(workspace_name)
            .await?;
        // The cache job mounts the workspace exactly as a runner does. Under
        // `Pooled` there is no per-workspace PVC to claim, so asking for one
        // left the pod Pending forever — and took the publish flow with it,
        // since that is what re-materializes a version's `WorkspaceDirectory`s.
        // The workspace affinity already co-locates this with any live runner,
        // so the two share one slot rather than racing from different nodes.
        let mode = workspace.effective_mode(ctx.config.default_workspace_mode);
        // Only `Dedicated` needs an indexer container here. A pooled slot is
        // hydrated when the agent publishes this pod's volume and flushed when
        // it unpublishes it, and that flush writes the archive *and* the
        // `WorkspaceDirectory` CRs — which is all this container was for. Worse,
        // it would be unschedulable: it runs under the per-workspace indexer
        // ServiceAccount, and pooled workspaces skip `apply_indexer_rbac`, so no
        // such account exists.
        let should_run_indexer = mode == WorkspaceMode::Dedicated
            && workspace.spec.indexer.is_some()
            && !indexer::is_pod_running(ctx, &workspace).await?;
        let affinity = Some(workspace_affinity::workspace_affinity(workspace_name));
        let mut pod_spec = PodSpec {
            containers: vec![],
            affinity,
            security_context: slot_volume::pod_security_context(mode),
            volumes: Some(vec![slot_volume::workspace_volume(
                workspace_name,
                mode,
                // Writes `__marimo__` caches into the workspace.
                false,
                slot_volume::SlotSources::from_workspace(Some(&workspace)),
                get_workspace_python_runtime(&workspace)?,
            )]),
            restart_policy: Some("Never".into()),
            ..Default::default()
        };

        let python_runtime = get_workspace_python_runtime(&workspace)?;

        let cache_container = self.cache_container(ctx, cache_job, python_runtime);
        if should_run_indexer {
            pod_spec
                .containers
                .push(self.indexer_container(ctx, &workspace, python_runtime)?);
            pod_spec.init_containers = Some(vec![cache_container]);
            pod_spec.service_account_name = Some(indexer::service_account_name(workspace_name));
        } else {
            pod_spec.containers.push(cache_container);
        }

        let pod_labels = workspace_affinity::workspace_label_map(workspace_name);
        let job = Job {
            metadata: ObjectMeta {
                name: Some(cache_job_name.to_string()),
                namespace: Some(namespace.to_string()),
                owner_references: Some(vec![cache_job.static_controller_owner_ref()?]),
                ..Default::default()
            },
            spec: Some(JobSpec {
                backoff_limit: cache_job.spec.backoff_limit,
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        labels: Some(pod_labels),
                        ..Default::default()
                    }),
                    spec: Some(pod_spec),
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        ctx.api_namespaced::<Job>(namespace).patch(&job).await
    }
}
