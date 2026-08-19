use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::{Runner, RunnerCommand, RunnerToken, Workspace, WorkspacePythonRuntime, prelude::*};

use crate::Config;
use crate::context::Context;
use crate::controllers::ingress::ingress_path;
use crate::controllers::runner_pod::{RunnerPodParams, TokenSource, build_runner_pod};
use crate::controllers::slot_volume::{self, SLOT_CSI_DRIVER};
use crate::controllers::workspace_affinity;

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
        python_runtime: WorkspacePythonRuntime,
    ) -> Result<PodApply, kubimo::Error> {
        let namespace = runner.require_namespace()?;
        let mode = workspace.effective_mode(ctx.config.default_workspace_mode);
        let sources = slot_volume::SlotSources::from_workspace(Some(workspace));
        let token = match runner.spec.token.as_ref() {
            Some(RunnerToken {
                value: Some(token), ..
            }) => TokenSource::Value(token),
            Some(RunnerToken {
                secret_ref: Some(secret_ref),
                ..
            }) => TokenSource::SecretEnv(secret_ref),
            _ => TokenSource::None,
        };
        let image = ctx.config.marimo_image(python_runtime).to_string();
        let pod = build_runner_pod(RunnerPodParams {
            name: runner.name()?.to_string(),
            namespace: namespace.to_string(),
            labels: self.pod_labels(runner)?,
            annotations: None,
            owner_reference: runner.static_controller_owner_ref()?,
            asset_url: ctx.config.runner_asset_url(&image),
            image,
            base_url: ingress_path(runner)?,
            token,
            log_level: runner.spec.log_level,
            port: runner_port(runner),
            origin: runner_origin(&ctx.config, runner),
            command: runner.spec.command,
            python_runtime,
            cpu: runner.spec.cpu.clone(),
            memory: runner.spec.memory.clone(),
            env: runner.spec.env.clone().unwrap_or_default(),
            env_from: runner.spec.env_from.clone(),
            mode,
            affinity: Some(workspace_affinity::workspace_affinity(
                &runner.spec.workspace,
            )),
            slot_volume: slot_volume::workspace_volume(
                &runner.spec.workspace,
                mode,
                // Render never mutates user data, so give it a read-only
                // bind and let one published version's slot be shared.
                matches!(runner.spec.command, RunnerCommand::Render),
                sources,
                python_runtime,
            ),
            extra_volumes: Vec::new(),
            sidecars: runner.spec.sidecars.clone(),
        });
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
                if matches!(&live, Ok(Some(live)) if runtime_class_drifted(live, &pod) || volumes_drifted(live, &pod) || asset_env_drifted(live, &pod))
                {
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

/// Check if both pods have the same python runtime volume attribute. This is needed because there
/// may still exist pods created before python runtimes were introduced. Pods are simplify recreated
/// if a drift is detected.
fn volumes_drifted(live: &Pod, desired: &Pod) -> bool {
    fn volume_python_runtime(pod: &Pod) -> Option<&str> {
        let spec = pod.spec.as_ref()?;
        let volume = spec.volumes.as_ref()?.iter().find_map(|vol| {
            let csi = vol.csi.as_ref()?;
            (csi.driver == SLOT_CSI_DRIVER).then_some(csi)
        })?;
        volume
            .volume_attributes
            .as_ref()?
            .get("python_runtime")
            .map(String::as_str)
    }
    volume_python_runtime(live) != volume_python_runtime(desired)
}

/// Whether the live pod's shared-asset env differs from the desired one. Env
/// is immutable on a live pod, so flipping `runner_asset_base_path` (or
/// moving the image tag while it is set) can only be honoured by replacement
/// — without this, every pre-existing pod 422-loops forever after the flip.
/// The env is baked into start.sh's marimo flags at boot, so an in-place
/// container restart could not apply it either.
fn asset_env_drifted(live: &Pod, desired: &Pod) -> bool {
    fn asset_url(pod: &Pod) -> Option<&str> {
        pod.spec
            .as_ref()?
            .containers
            .first()?
            .env
            .as_ref()?
            .iter()
            .find(|var| var.name == crate::controllers::runner_pod::ASSET_URL_ENV)?
            .value
            .as_deref()
    }
    asset_url(live) != asset_url(desired)
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
    use crate::controllers::runner_pod::sandbox_runtime_class;
    use crate::controllers::slot_volume;
    use kubimo::WorkspaceMode;
    use kubimo::k8s_openapi::api::core::v1::PodSpec;

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

    /// Env is immutable on a live pod, so enabling (or disabling) the shared
    /// asset origin can only converge by replacement — while a pod that
    /// already matches must never be a candidate, or any unrelated 422 would
    /// take a working notebook down.
    #[test]
    fn only_asset_env_drift_marks_a_pod_for_replacement() {
        use kubimo::k8s_openapi::api::core::v1::{Container, EnvVar};
        fn pod_with_asset_env(value: Option<&str>) -> Pod {
            Pod {
                spec: Some(PodSpec {
                    containers: vec![Container {
                        env: value.map(|value| {
                            vec![EnvVar {
                                name: crate::controllers::runner_pod::ASSET_URL_ENV.into(),
                                value: Some(value.to_string()),
                                ..Default::default()
                            }]
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
        let desired = pod_with_asset_env(Some("/marimo-assets/src-abc"));
        assert!(asset_env_drifted(&pod_with_asset_env(None), &desired));
        assert!(asset_env_drifted(
            &pod_with_asset_env(Some("/marimo-assets/src-old")),
            &desired
        ));
        assert!(!asset_env_drifted(
            &pod_with_asset_env(Some("/marimo-assets/src-abc")),
            &desired
        ));
        assert!(asset_env_drifted(
            &pod_with_asset_env(Some("/marimo-assets/src-abc")),
            &pod_with_asset_env(None),
        ));
        assert!(!asset_env_drifted(
            &pod_with_asset_env(None),
            &pod_with_asset_env(None)
        ));
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
