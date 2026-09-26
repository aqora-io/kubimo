use kubimo::k8s_openapi::api::batch::v1::{Job, JobSpec};
use kubimo::k8s_openapi::api::core::v1::{Container, PodSpec, PodTemplateSpec, VolumeMount};
use kubimo::kube::api::ObjectMeta;
use kubimo::{CacheJob, Workspace, prelude::*};

use crate::Config;
use crate::command::cmd;
use crate::context::Context;
use crate::controllers::runner_pod::{sandbox_runtime_class, with_sandbox_env};
use crate::controllers::slot_volume;
use crate::controllers::workspace_affinity;
use crate::resources::Resources;

use super::CacheJobReconciler;

impl CacheJobReconciler {
    fn cache_container(
        &self,
        config: &Config,
        cache_job: &CacheJob,
        workspace: &Workspace,
    ) -> Container {
        let workspace_name = cache_job.spec.workspace.clone();
        let mut command = cmd!["bash", "/setup/start.sh"];
        if let Some(log_level) = cache_job.spec.log_level.as_ref() {
            command.extend(cmd!["--log-level", log_level]);
        }
        command.push("cache".into());
        Container {
            name: "cache".into(),
            image: Some(config.marimo_image.clone()),
            resources: Resources::default()
                .cpu(cache_job.spec.cpu.clone())
                .memory(cache_job.spec.memory.clone())
                .into(),
            volume_mounts: Some(vec![VolumeMount {
                mount_path: slot_volume::MOUNT_DIR.into(),
                name: workspace_name,
                ..Default::default()
            }]),
            // Caches are built in the environments the workspace's runners
            // use, so with its sandbox backend.
            env: Some(with_sandbox_env(
                cache_job.spec.env.clone().unwrap_or_default(),
                workspace.spec.python_runtime.unwrap_or_default(),
            )),
            env_from: cache_job.spec.env_from.clone(),
            command: Some(command),
            ..Default::default()
        }
    }

    fn cache_pod_spec(
        &self,
        config: &Config,
        cache_job: &CacheJob,
        workspace: &Workspace,
    ) -> PodSpec {
        let workspace_name = &cache_job.spec.workspace;
        // The cache job mounts the workspace exactly as a runner does: the
        // slot is hydrated when the agent publishes this pod's volume and
        // flushed when it unpublishes it, and that flush writes the archive
        // *and* the `WorkspaceDirectory` CRs. The workspace affinity already
        // co-locates this with any live runner, so the two share one slot
        // rather than racing from different nodes.
        let affinity = Some(workspace_affinity::workspace_affinity(workspace_name));
        PodSpec {
            containers: vec![self.cache_container(config, cache_job, workspace)],
            affinity,
            volumes: Some(vec![slot_volume::workspace_volume(
                workspace_name,
                // Writes `__marimo__` caches into the workspace.
                false,
                slot_volume::SlotSources::from_workspace(Some(workspace)),
            )]),
            restart_policy: Some("Never".into()),
            // Sandboxed like a runner pod (see `runner_pod::build_runner_pod`):
            // the cache job builds environments and runs user notebooks.
            runtime_class_name: sandbox_runtime_class(),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
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
        let pod_spec = self.cache_pod_spec(&ctx.config, cache_job, &workspace);

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

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::CacheJobSpec;

    /// The cache job builds environments and runs user notebooks, so its pod
    /// gets the same sandbox as a runner pod.
    #[test]
    fn cache_job_pods_are_sandboxed_like_runner_pods() {
        let config = Config::test_default();
        let workspace = Workspace::new("bmow-test", Default::default());
        let cache_job = CacheJob::new(
            "bmocj-test",
            CacheJobSpec {
                workspace: "bmow-test".into(),
                ..Default::default()
            },
        );
        let spec = CacheJobReconciler.cache_pod_spec(&config, &cache_job, &workspace);
        assert_eq!(spec.runtime_class_name, sandbox_runtime_class());
        assert_eq!(spec.automount_service_account_token, Some(false));
        assert_eq!(spec.enable_service_links, Some(false));
        assert_eq!(
            spec.containers[0].image.as_deref(),
            Some(config.marimo_image.as_str())
        );
    }

    /// Caches are built with the workspace's backend, whatever the cache
    /// job's own env says.
    #[test]
    fn cache_jobs_run_the_workspaces_sandbox_backend() {
        use crate::controllers::runner_pod::SANDBOX_ENV;
        use kubimo::WorkspaceSpec;
        use kubimo::k8s_openapi::api::core::v1::EnvVar;
        let config = Config::test_default();
        let cache_job = CacheJob::new(
            "bmocj-test",
            CacheJobSpec {
                workspace: "bmow-test".into(),
                env: Some(vec![EnvVar {
                    name: SANDBOX_ENV.into(),
                    value: Some("bogus".into()),
                    ..Default::default()
                }]),
                ..Default::default()
            },
        );
        for (python_runtime, backend) in [
            (None, "uv"),
            (Some(kubimo::WorkspacePythonRuntime::Conda), "pixi"),
        ] {
            let workspace = Workspace::new(
                "bmow-test",
                WorkspaceSpec {
                    python_runtime,
                    ..Default::default()
                },
            );
            let spec = CacheJobReconciler.cache_pod_spec(&config, &cache_job, &workspace);
            let values: Vec<_> = spec.containers[0]
                .env
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|var| var.name == SANDBOX_ENV)
                .map(|var| var.value.as_deref())
                .collect();
            assert_eq!(values, vec![Some(backend)], "{python_runtime:?}");
        }
    }
}
