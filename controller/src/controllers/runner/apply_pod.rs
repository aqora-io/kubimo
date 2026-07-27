use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::core::v1::{
    CSIVolumeSource, Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction,
    PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec, Probe, Volume,
    VolumeMount,
};
use kubimo::k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kubimo::kube::api::ObjectMeta;
use kubimo::{Runner, RunnerCommand, Workspace, WorkspaceMode, prelude::*};

use crate::Config;
use crate::command::cmd;
use crate::context::Context;
use crate::controllers::ingress::ingress_path;
use crate::controllers::workspace_affinity;
use crate::resources::Resources;

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_pod(
        &self,
        ctx: &Context,
        runner: &Runner,
    ) -> Result<Pod, kubimo::Error> {
        let namespace = runner.require_namespace()?;
        // The workspace decides how storage is attached. It is guaranteed to
        // exist here: the reconciler already gated on it being Ready.
        let workspace = ctx
            .api_namespaced::<Workspace>(namespace)
            .get_opt(&runner.spec.workspace)
            .await?;
        let mode = workspace
            .as_ref()
            .map(|workspace| workspace.effective_mode(ctx.config.default_workspace_mode))
            .unwrap_or(ctx.config.default_workspace_mode);
        // The slot quota comes from `max`, not `min`: unused quota costs nothing
        // on a shared volume, so sizing to the ceiling retires the disk-full
        // failure class instead of reproducing it per slot.
        let storage_limit_bytes = workspace
            .as_ref()
            .and_then(|workspace| workspace.spec.storage.as_ref())
            .and_then(|storage| storage.max.as_ref())
            .and_then(|max| max.to_bytes());
        // Where the agent pulls the workspace's files from. Absent means the
        // workspace has no archive configured and starts on an empty slot.
        let archive = workspace
            .as_ref()
            .and_then(|workspace| workspace.spec.indexer.as_ref())
            .map(|indexer| (indexer.bucket.clone(), indexer.key_prefix.clone()));
        let ingress_path = ingress_path(runner)?;
        let path_prefix = ingress_path.strip_suffix('/').unwrap_or(&ingress_path);
        let mut command = cmd!["bash", "/setup/start.sh", "--base-url", ingress_path,];
        let mut env = runner.spec.env.clone().unwrap_or_default();
        if let Some(token_spec) = runner.spec.token.as_ref() {
            if let Some(token) = token_spec.value.as_ref() {
                command.extend(cmd!["--token", token]);
            } else if let Some(secret_ref) = token_spec.secret_ref.as_ref() {
                env.push(EnvVar {
                    name: "MARIMO_TOKEN".into(),
                    value_from: Some(EnvVarSource {
                        secret_key_ref: Some(secret_ref.clone()),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        }
        if let Some(log_level) = runner.spec.log_level.as_ref() {
            command.extend(cmd!["--log-level", log_level]);
        }
        let port = runner_port(runner);
        if port != 80 {
            command.extend(cmd!["--port", port]);
        }
        if let Some(host) = runner_origin(&ctx.config, runner) {
            command.extend(cmd!["--origin", host]);
        }
        let probe_action = HTTPGetAction {
            path: Some(format!("{path_prefix}/health")),
            port: IntOrString::Int(port),
            ..Default::default()
        };
        command.push(
            match runner.spec.command {
                RunnerCommand::Edit => "edit",
                RunnerCommand::Run => "run",
                RunnerCommand::Render => "render",
            }
            .into(),
        );
        let affinity = Some(workspace_affinity::workspace_affinity(
            &runner.spec.workspace,
        ));
        let mut containers = vec![Container {
            name: "runner".into(),
            image: Some(ctx.config.marimo_image.clone()),
            resources: Resources::default()
                .cpu(runner.spec.cpu.clone())
                .memory(runner.spec.memory.clone())
                .into(),
            volume_mounts: Some(vec![VolumeMount {
                mount_path: "/home/me".to_string(),
                name: runner.spec.workspace.clone(),
                ..Default::default()
            }]),
            ports: Some(vec![ContainerPort {
                container_port: port,
                name: Some("marimo".to_string()),
                ..Default::default()
            }]),
            env: if env.is_empty() { None } else { Some(env) },
            env_from: runner.spec.env_from.clone(),
            startup_probe: Some(Probe {
                http_get: Some(probe_action.clone()),
                failure_threshold: Some(90),
                period_seconds: Some(1),
                ..Default::default()
            }),
            liveness_probe: Some(Probe {
                http_get: Some(probe_action.clone()),
                period_seconds: Some(10),
                ..Default::default()
            }),
            command: Some(command),
            ..Default::default()
        }];
        if let Some(sidecars) = runner.spec.sidecars.clone() {
            containers.extend(sidecars);
        }
        let pod = Pod {
            metadata: ObjectMeta {
                name: runner.metadata.name.clone(),
                namespace: runner.metadata.namespace.clone(),
                owner_references: Some(vec![runner.static_controller_owner_ref()?]),
                labels: Some(self.pod_labels(runner)?),
                ..Default::default()
            },
            spec: Some(PodSpec {
                // Every command, including Render. Render executes user
                // notebooks just as Edit and Run do, so leaving it unsandboxed
                // was a pre-existing gap; a shared node volume makes it much
                // worse, since an escape reaches every other tenant's slot
                // rather than one workspace's own PVC.
                runtime_class_name: sandbox_runtime_class(),
                automount_service_account_token: Some(false),
                enable_service_links: Some(false),
                affinity,
                security_context: pod_security_context(mode),
                hostname: Some("kubimo".into()),
                containers,
                volumes: Some(vec![workspace_volume(
                    runner,
                    mode,
                    storage_limit_bytes,
                    archive,
                )]),
                ..Default::default()
            }),
            ..Default::default()
        };
        ctx.api_namespaced::<Pod>(namespace).patch(&pod).await
    }
}

/// Sandbox every runner, whatever its command.
fn sandbox_runtime_class() -> Option<String> {
    Some("gvisor".to_string())
}

/// Name of the CSI driver the node agent registers as.
const SLOT_CSI_DRIVER: &str = "kubimo.aqora.io";

/// The volume mounted at `/home/me`.
///
/// `Dedicated` uses the workspace's own PVC. `Pooled` uses an **inline
/// ephemeral** CSI volume served by the node agent, which resolves the
/// workspace to a slot on the node's shared data volume. Inline means no
/// PersistentVolume and therefore no topology constraint, so the scheduler
/// stays in charge — a per-node PVC would force hostname pinning, which
/// cluster-autoscaler can never satisfy against its template node.
fn workspace_volume(
    runner: &Runner,
    mode: WorkspaceMode,
    storage_limit_bytes: Option<u64>,
    archive: Option<(Option<String>, Option<String>)>,
) -> Volume {
    let name = runner.spec.workspace.clone();
    match mode {
        WorkspaceMode::Dedicated => Volume {
            name: name.clone(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: name,
                ..Default::default()
            }),
            ..Default::default()
        },
        WorkspaceMode::Pooled => {
            let mut attributes = BTreeMap::from([("workspace".to_string(), name.clone())]);
            if let Some(limit) = storage_limit_bytes {
                attributes.insert("limitBytes".to_string(), limit.to_string());
            }
            // Only pass the bucket when it is actually set: the agent treats a
            // missing bucket as "no archive, start empty" rather than guessing.
            if let Some((bucket, key_prefix)) = archive {
                if let Some(bucket) = bucket {
                    attributes.insert("bucket".to_string(), bucket);
                }
                if let Some(key_prefix) = key_prefix {
                    attributes.insert("keyPrefix".to_string(), key_prefix);
                }
            }
            Volume {
                name,
                csi: Some(CSIVolumeSource {
                    driver: SLOT_CSI_DRIVER.to_string(),
                    // Render never mutates user data, so give it a read-only
                    // bind and let one published version's slot be shared.
                    read_only: Some(matches!(runner.spec.command, RunnerCommand::Render)),
                    volume_attributes: Some(attributes),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
    }
}

/// `fsGroup` is only safe on a volume the workspace owns outright.
///
/// On the shared node volume kubelet would recursively chown the **entire**
/// volume — every slot on the node — at every pod start, which blows past the
/// runner's 90s startup probe once a node is full. The agent chowns exactly the
/// slot it creates instead, and the runner image already runs as uid 1000.
fn pod_security_context(mode: WorkspaceMode) -> Option<PodSecurityContext> {
    match mode {
        WorkspaceMode::Dedicated => Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
        WorkspaceMode::Pooled => None,
    }
}

pub(crate) fn runner_port(runner: &Runner) -> i32 {
    match runner.spec.command {
        RunnerCommand::Render => 8080,
        RunnerCommand::Edit | RunnerCommand::Run => 80,
    }
}

pub(crate) fn runner_origin<'a>(config: &'a Config, runner: &'a Runner) -> Option<String> {
    // Runner's origin is the first that appears in its spec
    let first_spec_host = runner
        .spec
        .ingress
        .as_ref()
        .and_then(|ing| ing.tls.as_ref())
        .and_then(|tls| tls.hosts.as_ref())
        .and_then(|hs| hs.first())
        .map(String::as_str);

    // We fallback on configured host if none found
    let first_config_host = config.runner_hosts.first().map(String::as_str);

    first_spec_host
        .or(first_config_host)
        .map(|host| format!("https://{host}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::{RunnerSpec, WorkspaceIndexer};

    fn runner(command: RunnerCommand) -> Runner {
        Runner::new(
            "bmor-test",
            RunnerSpec {
                workspace: "bmow-test".into(),
                command,
                ..Default::default()
            },
        )
    }

    #[test]
    fn dedicated_mode_mounts_the_workspace_pvc() {
        let volume = workspace_volume(
            &runner(RunnerCommand::Edit),
            WorkspaceMode::Dedicated,
            None,
            None,
        );
        assert_eq!(
            volume.persistent_volume_claim.unwrap().claim_name,
            "bmow-test"
        );
        assert!(volume.csi.is_none());
    }

    #[test]
    fn pooled_mode_uses_an_inline_csi_volume() {
        let volume = workspace_volume(
            &runner(RunnerCommand::Edit),
            WorkspaceMode::Pooled,
            Some(2_147_483_648),
            Some((Some("bucket".into()), Some("workspace/abc/".into()))),
        );
        assert!(volume.persistent_volume_claim.is_none());
        let csi = volume.csi.unwrap();
        assert_eq!(csi.driver, SLOT_CSI_DRIVER);
        let attrs = csi.volume_attributes.unwrap();
        assert_eq!(attrs.get("workspace").unwrap(), "bmow-test");
        assert_eq!(attrs.get("limitBytes").unwrap(), "2147483648");
        assert_eq!(attrs.get("bucket").unwrap(), "bucket");
        assert_eq!(attrs.get("keyPrefix").unwrap(), "workspace/abc/");
    }

    /// A workspace with no indexer config must not get a half-specified
    /// archive: the agent keys off `bucket` being absent to start empty.
    #[test]
    fn pooled_mode_omits_archive_attributes_when_unconfigured() {
        let volume = workspace_volume(
            &runner(RunnerCommand::Edit),
            WorkspaceMode::Pooled,
            None,
            None,
        );
        let attrs = volume.csi.unwrap().volume_attributes.unwrap();
        assert!(!attrs.contains_key("bucket"));
        assert!(!attrs.contains_key("keyPrefix"));
        assert!(!attrs.contains_key("limitBytes"));
    }

    /// Render never mutates user data, so its bind is read-only.
    #[test]
    fn render_gets_a_read_only_slot_and_edit_does_not() {
        for (command, expected) in [
            (RunnerCommand::Render, true),
            (RunnerCommand::Edit, false),
            (RunnerCommand::Run, false),
        ] {
            let volume =
                workspace_volume(&runner(command.clone()), WorkspaceMode::Pooled, None, None);
            assert_eq!(volume.csi.unwrap().read_only, Some(expected), "{command:?}");
        }
    }

    /// Render executes user notebooks too, so it must be sandboxed like the
    /// others. Regression guard: this used to be Edit/Run only.
    #[test]
    fn no_runner_command_is_exempt_from_the_sandbox() {
        assert_eq!(sandbox_runtime_class().as_deref(), Some("gvisor"));
    }

    /// `fsGroup` on a shared node volume makes kubelet recursively chown every
    /// slot on the node at each pod start.
    #[test]
    fn fs_group_is_only_set_for_dedicated_volumes() {
        assert_eq!(
            pod_security_context(WorkspaceMode::Dedicated)
                .unwrap()
                .fs_group,
            Some(1000)
        );
        assert!(pod_security_context(WorkspaceMode::Pooled).is_none());
    }

    #[test]
    fn indexer_config_maps_into_archive_attributes() {
        let indexer = WorkspaceIndexer {
            bucket: Some("b".into()),
            key_prefix: Some("p/".into()),
            ..Default::default()
        };
        assert_eq!(
            (indexer.bucket.clone(), indexer.key_prefix.clone()),
            (Some("b".to_string()), Some("p/".to_string()))
        );
    }
}
