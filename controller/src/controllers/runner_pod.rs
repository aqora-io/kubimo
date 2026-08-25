//! Construction of a marimo runner pod, in either of its two lives.
//!
//! Shared by the runner reconciler (a cold pod bound to its workspace at
//! build time) and the pool controller (a warm pod pre-booted on an anonymous
//! slot). They must produce the same pod shape — sandbox, probes, mounts,
//! start.sh contract — so a difference between them is a bug, never a design
//! choice: a claimed warm pod *is* the runner's pod from then on.

use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::core::v1::{
    Affinity, Container, ContainerPort, EnvFromSource, EnvVar, EnvVarSource, HTTPGetAction, Pod,
    PodSpec, Probe, SecretKeySelector, Volume, VolumeMount,
};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kubimo::k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kubimo::kube::api::ObjectMeta;
use kubimo::{
    CpuQuantity, LogLevel, Requirement, RunnerCommand, StorageQuantity, WorkspacePythonRuntime,
};

use crate::command::cmd;
use crate::controllers::slot_volume;
use crate::resources::Resources;

/// How the shared static-asset URL reaches start.sh. An env var, never a
/// flag: an older image must start normally rather than crash on an unknown
/// argument (same contract as the claim marker).
pub(crate) const ASSET_URL_ENV: &str = "KUBIMO_ASSET_URL";

/// How the marimo access token reaches start.sh.
pub(crate) enum TokenSource<'a> {
    /// As a `--token` argument, from `spec.token.value` or a pool's minted
    /// token.
    Value(&'a str),
    /// As the `MARIMO_TOKEN` environment variable, resolved by kubelet from
    /// `spec.token.secretRef`.
    SecretEnv(&'a SecretKeySelector),
    None,
}

pub(crate) struct RunnerPodParams<'a> {
    pub name: String,
    pub namespace: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: Option<BTreeMap<String, String>>,
    pub owner_reference: OwnerReference,
    pub image: String,
    pub base_url: String,
    /// The shared static-asset URL for this pod's image
    /// (`Config::runner_asset_url`), reaching start.sh as `KUBIMO_ASSET_URL`.
    /// `None` leaves marimo serving assets under its own base-url.
    pub asset_url: Option<String>,
    pub token: TokenSource<'a>,
    pub log_level: Option<LogLevel>,
    pub port: i32,
    pub origin: Option<String>,
    pub command: RunnerCommand,
    /// Drives the startup probe budget: conda's dependency sync runs before
    /// marimo serves.
    pub python_runtime: WorkspacePythonRuntime,
    pub cpu: Option<Requirement<CpuQuantity>>,
    pub memory: Option<Requirement<StorageQuantity>>,
    pub env: Vec<EnvVar>,
    pub env_from: Option<Vec<EnvFromSource>>,
    /// `None` for warm pods: they belong to no workspace yet, so there is
    /// nothing to co-locate with.
    pub affinity: Option<Affinity>,
    /// The volume mounted at `/home/me` — a workspace's slot, or a warm pod's
    /// anonymous slot.
    pub slot_volume: Volume,
    /// Additional volumes referenced by sidecars (a warm pod's claim Secret).
    /// Never mounted into the runner container.
    pub extra_volumes: Vec<Volume>,
    pub sidecars: Option<Vec<Container>>,
}

pub(crate) fn build_runner_pod(params: RunnerPodParams<'_>) -> Pod {
    let path_prefix = params
        .base_url
        .strip_suffix('/')
        .unwrap_or(&params.base_url)
        .to_string();
    let mut command = cmd!["bash", "/setup/start.sh", "--base-url", params.base_url];
    let mut env = params.env;
    if let Some(asset_url) = params.asset_url {
        env.push(EnvVar {
            name: ASSET_URL_ENV.into(),
            value: Some(asset_url),
            ..Default::default()
        });
    }
    match params.token {
        TokenSource::Value(token) => command.extend(cmd!["--token", token]),
        TokenSource::SecretEnv(secret_ref) => env.push(EnvVar {
            name: "MARIMO_TOKEN".into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(secret_ref.clone()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        TokenSource::None => {}
    }
    if let Some(log_level) = params.log_level.as_ref() {
        command.extend(cmd!["--log-level", log_level]);
    }
    let port = params.port;
    if port != 80 {
        command.extend(cmd!["--port", port]);
    }
    if let Some(origin) = params.origin.as_ref() {
        command.extend(cmd!["--origin", origin]);
    }
    command.push(
        match params.command {
            RunnerCommand::Edit => "edit",
            RunnerCommand::Run => "run",
            RunnerCommand::Render => "render",
        }
        .into(),
    );
    let probe_action = HTTPGetAction {
        path: Some(format!("{path_prefix}/health")),
        port: IntOrString::Int(port),
        ..Default::default()
    };
    let volume_name = params.slot_volume.name.clone();
    let mut containers = vec![Container {
        name: "runner".into(),
        image: Some(params.image),
        resources: Resources::default()
            .cpu(params.cpu)
            .memory(params.memory)
            .into(),
        volume_mounts: Some(vec![VolumeMount {
            mount_path: slot_volume::MOUNT_DIR.to_string(),
            name: volume_name,
            ..Default::default()
        }]),
        ports: Some(vec![ContainerPort {
            container_port: port,
            name: Some("marimo".to_string()),
            ..Default::default()
        }]),
        env: if env.is_empty() { None } else { Some(env) },
        env_from: params.env_from,
        startup_probe: Some(Probe {
            http_get: Some(probe_action.clone()),
            failure_threshold: Some(match params.python_runtime {
                WorkspacePythonRuntime::Uv => 90,
                WorkspacePythonRuntime::Conda => 300,
            }),
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
    if let Some(sidecars) = params.sidecars {
        containers.extend(sidecars);
    }
    let mut volumes = vec![params.slot_volume];
    volumes.extend(params.extra_volumes);
    Pod {
        metadata: ObjectMeta {
            name: Some(params.name),
            namespace: Some(params.namespace),
            owner_references: Some(vec![params.owner_reference]),
            labels: Some(params.labels),
            annotations: params.annotations,
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
            affinity: params.affinity,
            // No `fsGroup`: kubelet would recursively chown the **entire**
            // shared node volume — every slot on the node — at every pod
            // start. The agent chowns exactly the slot it creates instead,
            // and the runner image already runs as uid 1000.
            security_context: None,
            hostname: Some("kubimo".into()),
            containers,
            volumes: Some(volumes),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Sandbox every runner, whatever its command.
pub(crate) fn sandbox_runtime_class() -> Option<String> {
    Some("gvisor".to_string())
}
