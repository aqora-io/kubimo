use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{Container, PodSpec, PodTemplateSpec, VolumeMount};
use kubimo::kube::api::ObjectMeta;
use kubimo::{CacheJob, Workspace, WorkspacePythonRuntime, prelude::*};

use crate::command::cmd;
use crate::context::Context;
use crate::controllers::slot_volume;
use crate::controllers::workspace_affinity;
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
                mount_path: slot_volume::MOUNT_DIR.into(),
                name: workspace_name,
                ..Default::default()
            }]),
            env: cache_job.spec.env.clone(),
            env_from: cache_job.spec.env_from.clone(),
            command: Some(command),
            ..Default::default()
        }
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
        // The cache job mounts the workspace exactly as a runner does: the
        // slot is hydrated when the agent publishes this pod's volume and
        // flushed when it unpublishes it, and that flush writes the archive
        // *and* the `WorkspaceDirectory` CRs. The workspace affinity already
        // co-locates this with any live runner, so the two share one slot
        // rather than racing from different nodes.
        let affinity = Some(workspace_affinity::workspace_affinity(workspace_name));
        let python_runtime = workspace.spec.python_runtime.unwrap_or_default();
        let pod_spec = PodSpec {
            containers: vec![self.cache_container(ctx, cache_job, python_runtime)],
            affinity,
            volumes: Some(vec![slot_volume::workspace_volume(
                workspace_name,
                // Writes `__marimo__` caches into the workspace.
                false,
                slot_volume::SlotSources::from_workspace(Some(&workspace)),
                python_runtime,
            )]),
            restart_policy: Some("Never".into()),
            ..Default::default()
        };

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
