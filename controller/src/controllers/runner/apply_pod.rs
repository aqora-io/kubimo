use kubimo::k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction, Pod, PodSpec, Probe, VolumeMount,
};
use kubimo::k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kubimo::kube::api::ObjectMeta;
use kubimo::{Runner, RunnerCommand, Workspace, prelude::*};

use crate::Config;
use crate::command::cmd;
use crate::context::Context;
use crate::controllers::ingress::ingress_path;
use crate::controllers::slot_volume;
use crate::controllers::workspace_affinity;
use crate::resources::Resources;

use super::RunnerReconciler;

/// What an apply did to the live pod.
///
/// `Replaced` is the one outcome the caller has to act on: the drifted pod has
/// been deleted and nothing has recreated it yet, so the reconcile has to come
/// back rather than wait for a change.
pub(crate) enum PodApply {
    Applied,
    Replaced,
}

impl RunnerReconciler {
    pub(crate) async fn apply_pod(
        &self,
        ctx: &Context,
        runner: &Runner,
        // The workspace decides how storage is attached. Passed in rather than
        // fetched again: the caller has already read it to gate on Ready, and a
        // second GET could see a different generation than the gate did.
        workspace: &Workspace,
    ) -> Result<PodApply, kubimo::Error> {
        let namespace = runner.require_namespace()?;
        let mode = workspace.effective_mode(ctx.config.default_workspace_mode);
        let sources = slot_volume::SlotSources::from_workspace(Some(workspace));
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
                security_context: slot_volume::pod_security_context(mode),
                hostname: Some("kubimo".into()),
                containers,
                volumes: Some(vec![slot_volume::workspace_volume(
                    &runner.spec.workspace,
                    mode,
                    // Render never mutates user data, so give it a read-only
                    // bind and let one published version's slot be shared.
                    matches!(runner.spec.command, RunnerCommand::Render),
                    sources,
                )]),
                ..Default::default()
            }),
            ..Default::default()
        };
        match ctx.api_namespaced::<Pod>(namespace).patch(&pod).await {
            Err(err) if super::is_invalid_request(&err) => {
                // A live pod's spec is almost entirely immutable, so an apply that needs to
                // change one of those fields — the sandbox runtimeClassName on a pod created
                // before Render was sandboxed — can only be honoured by replacement. But a
                // 422 is also what any other pod validation failure looks like (say, a
                // Runner whose resources produce requests over limits), and deleting a
                // working pod over one of those would take a user's notebook down for a
                // bad input. So fetch the live pod and delete only on the one drift this
                // replacement exists for; every other 422, and any failure to fetch,
                // propagates untouched — those never converge on their own, and the
                // caller's backoff is what keeps them from spinning. Pods carry a
                // termination grace period, so recreation must wait for the next reconcile
                // rather than racing the delete.
                let live = ctx
                    .api_namespaced::<Pod>(namespace)
                    .get_opt(runner.name()?)
                    .await;
                if matches!(&live, Ok(Some(live)) if runtime_class_drifted(live, &pod)) {
                    ctx.api_namespaced::<Pod>(namespace)
                        .delete_opt(runner.name()?)
                        .await?;
                    return Ok(PodApply::Replaced);
                }
                Err(err)
            }
            result => result.map(|_| PodApply::Applied),
        }
    }
}

/// Whether the live pod's runtime class differs from the desired one — the one
/// immutable-field change a pod is deliberately replaced over. Other drifts, if
/// ever introduced, should be added here on purpose rather than deleting on any
/// 422.
fn runtime_class_drifted(live: &Pod, desired: &Pod) -> bool {
    fn class(pod: &Pod) -> Option<&str> {
        pod.spec
            .as_ref()
            .and_then(|spec| spec.runtime_class_name.as_deref())
    }
    class(live) != class(desired)
}

/// Sandbox every runner, whatever its command.
fn sandbox_runtime_class() -> Option<String> {
    Some("gvisor".to_string())
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
    use crate::controllers::slot_volume;
    use kubimo::WorkspaceMode;

    /// Render never mutates user data, so its bind is read-only — which also
    /// lets one published version's slot be shared between renderers.
    #[test]
    fn render_gets_a_read_only_slot_and_edit_does_not() {
        for (command, expected) in [
            (RunnerCommand::Render, true),
            (RunnerCommand::Edit, false),
            (RunnerCommand::Run, false),
        ] {
            let volume = slot_volume::workspace_volume(
                "bmow-test",
                WorkspaceMode::Pooled,
                matches!(command, RunnerCommand::Render),
                Default::default(),
            );
            assert_eq!(volume.csi.unwrap().read_only, Some(expected), "{command:?}");
        }
    }

    /// Render executes user notebooks too, so it must be sandboxed like the
    /// others. Regression guard: this used to be Edit/Run only.
    #[test]
    fn no_runner_command_is_exempt_from_the_sandbox() {
        assert_eq!(sandbox_runtime_class().as_deref(), Some("gvisor"));
    }

    fn pod_with_runtime_class(class: Option<&str>) -> Pod {
        Pod {
            spec: Some(PodSpec {
                runtime_class_name: class.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Only a pod whose live runtime class differs from the desired one is a
    /// replacement candidate; a pod that already matches must never be, or any
    /// unrelated 422 would take a working notebook down.
    #[test]
    fn only_runtime_class_drift_marks_a_pod_for_replacement() {
        let desired = pod_with_runtime_class(Some("gvisor"));
        assert!(runtime_class_drifted(
            &pod_with_runtime_class(None),
            &desired
        ));
        assert!(runtime_class_drifted(&Pod::default(), &desired));
        assert!(!runtime_class_drifted(
            &pod_with_runtime_class(Some("gvisor")),
            &desired
        ));
    }
}
