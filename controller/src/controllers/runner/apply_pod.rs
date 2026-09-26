use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::{Runner, RunnerCommand, RunnerToken, Workspace, prelude::*};

use crate::Config;
use crate::context::Context;
use crate::controllers::ingress::ingress_path;
use crate::controllers::runner_pod::{RunnerPodParams, TokenSource, build_runner_pod};
use crate::controllers::slot_volume::{self, SLOT_CSI_DRIVER};
use crate::controllers::workspace_affinity;

use super::RunnerReconciler;

/// What an apply did to the live pod.
///
/// `Replaced` and `AwaitingTermination` are both outcomes the caller has to
/// act on rather than wait for a change: a drifted pod has either just been
/// deleted, or was already deleted by an earlier reconcile and hasn't finished
/// terminating yet. Either way nothing has recreated it, so the reconcile has
/// to come back.
pub(crate) enum PodApply {
    Applied,
    Replaced,
    AwaitingTermination,
}

impl RunnerReconciler {
    pub(crate) async fn apply_pod(
        &self,
        ctx: &Context,
        runner: &Runner,
        // The workspace supplies the slot's sources. Passed in rather than
        // fetched again: the caller has already read it to gate on Ready, and a
        // second GET could see a different generation than the gate did.
        workspace: &Workspace,
    ) -> Result<PodApply, kubimo::Error> {
        let namespace = runner.require_namespace()?;
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
        let image = ctx.config.marimo_image.clone();
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
            python_runtime: workspace.spec.python_runtime.unwrap_or_default(),
            cpu: runner.spec.cpu.clone(),
            memory: runner.spec.memory.clone(),
            env: runner.spec.env.clone().unwrap_or_default(),
            env_from: runner.spec.env_from.clone(),
            affinity: Some(workspace_affinity::workspace_affinity(
                &runner.spec.workspace,
            )),
            slot_volume: slot_volume::workspace_volume(
                &runner.spec.workspace,
                // Render never mutates user data, so give it a read-only
                // bind and let one published version's slot be shared.
                matches!(runner.spec.command, RunnerCommand::Render),
                sources,
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
                if let Ok(Some(live)) = &live
                    && (runtime_class_drifted(live, &pod)
                        || retired_slot_attribute_drifted(live, &pod)
                        || asset_env_drifted(live, &pod)
                        || sandbox_env_drifted(live, &pod))
                {
                    if is_terminating(live) {
                        // An earlier reconcile already deleted this pod for
                        // this same drift; it just hasn't gone yet. Deleting
                        // it again would be a no-op, and logging a fresh
                        // replacement would be wrong: nothing new happened.
                        return Ok(PodApply::AwaitingTermination);
                    }
                    ctx.api_namespaced::<Pod>(namespace)
                        .delete_opt(runner.name()?)
                        .await?;
                    tracing::info!(
                        runner = runner.name()?,
                        "replaced a drifted pod; requeuing to recreate it"
                    );
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

/// Slot-volume attributes older controllers set and this one never does. Volumes are
/// immutable, so a live pod carrying one 422s every apply; this turns that into exactly
/// one replacement. Append-only: a straggler pod would otherwise 422 forever.
const RETIRED_SLOT_ATTRIBUTES: &[&str] = &["python_runtime"];

/// Whether any [`RETIRED_SLOT_ATTRIBUTES`] entry differs between the live
/// pod's slot volume and the desired one's.
fn retired_slot_attribute_drifted(live: &Pod, desired: &Pod) -> bool {
    fn slot_attribute<'a>(pod: &'a Pod, key: &str) -> Option<&'a str> {
        let spec = pod.spec.as_ref()?;
        let volume = spec.volumes.as_ref()?.iter().find_map(|vol| {
            let csi = vol.csi.as_ref()?;
            (csi.driver == SLOT_CSI_DRIVER).then_some(csi)
        })?;
        volume
            .volume_attributes
            .as_ref()?
            .get(key)
            .map(String::as_str)
    }
    RETIRED_SLOT_ATTRIBUTES
        .iter()
        .any(|key| slot_attribute(live, key) != slot_attribute(desired, key))
}

/// Whether the live pod's shared-asset env differs from the desired one. Env
/// is immutable on a live pod, so flipping `runner_asset_base_path` (or
/// moving the image tag while it is set) can only be honoured by replacement
/// — without this, every pre-existing pod 422-loops forever after the flip.
/// The env is baked into start.sh's marimo flags at boot, so an in-place
/// container restart could not apply it either.
fn asset_env_drifted(live: &Pod, desired: &Pod) -> bool {
    let name = crate::controllers::runner_pod::ASSET_URL_ENV;
    runner_env(live, name) != runner_env(desired, name)
}

/// Whether the live pod runs another sandbox backend than the desired one.
/// start.sh bakes it into marimo's `--sandbox` at boot and env is immutable,
/// so only a replacement applies it; pods from before the runtime chose a
/// backend carry none, and are replaced exactly once.
fn sandbox_env_drifted(live: &Pod, desired: &Pod) -> bool {
    let name = crate::controllers::runner_pod::SANDBOX_ENV;
    runner_env(live, name) != runner_env(desired, name)
}

/// The value of the runner container's env var `name`.
fn runner_env<'a>(pod: &'a Pod, name: &str) -> Option<&'a str> {
    pod.spec
        .as_ref()?
        .containers
        .first()?
        .env
        .as_ref()?
        .iter()
        .find(|var| var.name == name)?
        .value
        .as_deref()
}

/// Whether the live pod has already been asked to terminate — i.e. an
/// earlier reconcile already deleted it for one of the drifts above, and it
/// just hasn't finished going away yet.
fn is_terminating(pod: &Pod) -> bool {
    pod.metadata.deletion_timestamp.is_some()
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

    /// start.sh bakes the backend into marimo at boot, so a pod running another
    /// runtime than the workspace's is replaced, and a pod from before the
    /// runtime chose a backend, which carries none, is replaced once. A pod
    /// that already matches never is.
    #[test]
    fn only_sandbox_env_drift_marks_a_pod_for_replacement() {
        use crate::controllers::runner_pod::with_sandbox_env;
        use kubimo::WorkspacePythonRuntime::{Conda, Uv};
        use kubimo::k8s_openapi::api::core::v1::Container;
        fn pod_with_env(env: Option<Vec<kubimo::k8s_openapi::api::core::v1::EnvVar>>) -> Pod {
            Pod {
                spec: Some(PodSpec {
                    containers: vec![Container {
                        env,
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
        let pod = |runtime| pod_with_env(Some(with_sandbox_env(Vec::new(), runtime)));
        assert!(sandbox_env_drifted(&pod_with_env(None), &pod(Uv)));
        assert!(sandbox_env_drifted(&pod(Conda), &pod(Uv)));
        assert!(sandbox_env_drifted(&pod(Uv), &pod(Conda)));
        assert!(!sandbox_env_drifted(&pod(Uv), &pod(Uv)));
        assert!(!sandbox_env_drifted(&pod(Conda), &pod(Conda)));
    }

    /// A live pod whose slot volume still carries a retired attribute, whatever
    /// its value, 422s on every apply and must be replaced; one that differs
    /// only in an attribute still set (a changed quota) is some other 422 and
    /// must be left alone.
    #[test]
    fn only_a_retired_slot_attribute_marks_a_pod_for_replacement() {
        fn pod_with_slot_attributes(attributes: &[(&str, &str)]) -> Pod {
            let mut volume = slot_volume::workspace_volume(
                "bmow-test",
                false,
                slot_volume::SlotSources::default(),
            );
            volume
                .csi
                .as_mut()
                .unwrap()
                .volume_attributes
                .get_or_insert_default()
                .extend(
                    attributes
                        .iter()
                        .map(|(key, value)| (key.to_string(), value.to_string())),
                );
            Pod {
                spec: Some(PodSpec {
                    volumes: Some(vec![volume]),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
        let desired = pod_with_slot_attributes(&[("limitBytes", "2147483648")]);
        for python_runtime in ["Uv", "Conda"] {
            assert!(
                retired_slot_attribute_drifted(
                    &pod_with_slot_attributes(&[
                        ("limitBytes", "2147483648"),
                        ("python_runtime", python_runtime),
                    ]),
                    &desired
                ),
                "{python_runtime}"
            );
        }
        assert!(!retired_slot_attribute_drifted(
            &pod_with_slot_attributes(&[("limitBytes", "2147483648")]),
            &desired
        ));
        assert!(!retired_slot_attribute_drifted(
            &pod_with_slot_attributes(&[("limitBytes", "1073741824")]),
            &desired
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

    /// A pod an earlier reconcile already deleted for a retired attribute is
    /// still Terminating, not gone; it must read as such so a second reconcile
    /// waits instead of deleting (a no-op) and logging the replacement again.
    #[test]
    fn is_terminating_reflects_the_live_pods_deletion_timestamp() {
        use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use kubimo::k8s_openapi::jiff::Timestamp;
        let deleting = Pod {
            metadata: kubimo::kube::api::ObjectMeta {
                deletion_timestamp: Some(Time(Timestamp::UNIX_EPOCH)),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(is_terminating(&deleting));
        assert!(!is_terminating(&Pod::default()));
    }
}
