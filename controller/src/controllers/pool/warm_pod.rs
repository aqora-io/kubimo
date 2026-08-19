//! Minting and building warm pods.
//!
//! A warm pod is a runner pod with no runner: same image, sandbox, probes and
//! start.sh contract, but booted on an anonymous slot with a base-url and
//! token minted here. Both are baked into the pod command — marimo cannot
//! change them once serving — and recorded as annotations, which is what the
//! runner reconciler reads back at claim time.

use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::core::v1::{EnvVar, Pod, Secret, SecretVolumeSource, Volume};
use kubimo::kube::api::ObjectMeta;
use kubimo::pool::{
    CLAIM_MARKER_ENV, CLAIM_MARKER_RELATIVE_PATH, POOL_LABEL, POOL_STATE_LABEL, POOL_STATE_WARM,
    POOL_TEMPLATE_HASH_ANNOTATION, WARM_BASE_URL_ANNOTATION, WARM_TOKEN_ANNOTATION,
};
use kubimo::{Pool, WorkspaceMode, prelude::*};
use sha2::{Digest, Sha256};

use crate::Config;
use crate::controllers::ingress::ingress_path_from_name;
use crate::controllers::runner_pod::{RunnerPodParams, TokenSource, build_runner_pod};
use crate::controllers::slot_volume;

/// The volume name pool sidecar templates mount the per-pod claim Secret by.
pub(crate) const CLAIM_VOLUME_NAME: &str = "claim";

pub(crate) struct WarmPodIdentity {
    pub name: String,
    pub token: String,
    pub base_url: String,
}

pub(crate) fn mint_identity(pool_name: &str) -> WarmPodIdentity {
    let name = format!("{pool_name}-{}", hex(rand::random::<[u8; 4]>()));
    WarmPodIdentity {
        // The path is derived from the pod name, which is already unique, so
        // routing collisions reduce to name collisions.
        base_url: ingress_path_from_name(&name),
        token: hex(rand::random::<[u8; 16]>()),
        name,
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn claim_secret_name(pod_name: &str) -> String {
    format!("{pod_name}-claim")
}

/// The empty per-pod Secret sidecars read their claim-time configuration (the
/// runner's api key) from. Owned by the pod, so it is collected with it —
/// warm, retired or claimed alike.
pub(crate) fn claim_secret(pod: &Pod) -> kubimo::Result<Secret> {
    Ok(Secret {
        metadata: ObjectMeta {
            name: Some(claim_secret_name(pod.name()?)),
            namespace: pod.metadata.namespace.clone(),
            owner_references: Some(vec![pod.static_controller_owner_ref()?]),
            ..Default::default()
        },
        ..Default::default()
    })
}

/// Everything that decides what a warm pod *is*, hashed so drift can be
/// detected without diffing pod specs. Deliberately excludes the minted
/// name/token/base-url (random per pod) and `replicas` (a sizing knob, not a
/// shape).
pub(crate) fn template_hash(config: &Config, pool: &Pool) -> String {
    let image = config.marimo_image(pool.spec.python_runtime.unwrap_or_default());
    let mut fingerprint = serde_json::json!({
        "image": image,
        "command": pool.spec.command,
        "pythonRuntime": pool.spec.python_runtime.unwrap_or_default(),
        "logLevel": pool.spec.log_level,
        "cpu": pool.spec.cpu,
        "memory": pool.spec.memory,
        "env": pool.spec.env,
        "sidecars": pool.spec.sidecars,
        "s3SecretName": pool.spec.s3_secret_name,
        "storage": pool.spec.storage,
        "origin": config.runner_hosts.first(),
    });
    // Inserted only when configured: flipping the asset origin on (or off)
    // must retire warm pods so they re-mint with the right KUBIMO_ASSET_URL,
    // but a controller upgrade with the feature off must not churn the fleet.
    if let Some(asset_url) = config.runner_asset_url(image) {
        fingerprint["assetUrl"] = asset_url.into();
    }
    // serde_json maps are sorted, so the serialization is canonical.
    hex(Sha256::digest(fingerprint.to_string()))
}

pub(crate) fn build_warm_pod(
    config: &Config,
    pool: &Pool,
    identity: &WarmPodIdentity,
) -> kubimo::Result<Pod> {
    let pool_name = pool.name()?;
    let python_runtime = pool.spec.python_runtime.unwrap_or_default();
    let image = config.marimo_image(python_runtime).to_string();
    let mut env = pool.spec.env.clone().unwrap_or_default();
    env.push(EnvVar {
        name: CLAIM_MARKER_ENV.to_string(),
        value: Some(format!("/home/me/{CLAIM_MARKER_RELATIVE_PATH}")),
        ..Default::default()
    });
    Ok(build_runner_pod(RunnerPodParams {
        name: identity.name.clone(),
        namespace: pool.require_namespace()?.to_string(),
        // No runner name and no workspace label: the pod matches no Service
        // selector and attracts no workspace affinity until it is claimed.
        labels: BTreeMap::from([
            (POOL_LABEL.to_string(), pool_name.to_string()),
            (POOL_STATE_LABEL.to_string(), POOL_STATE_WARM.to_string()),
        ]),
        annotations: Some(BTreeMap::from([
            (
                WARM_BASE_URL_ANNOTATION.to_string(),
                identity.base_url.clone(),
            ),
            (WARM_TOKEN_ANNOTATION.to_string(), identity.token.clone()),
            (
                POOL_TEMPLATE_HASH_ANNOTATION.to_string(),
                template_hash(config, pool),
            ),
        ])),
        owner_reference: pool.static_controller_owner_ref()?,
        asset_url: config.runner_asset_url(&image),
        image,
        base_url: identity.base_url.clone(),
        token: TokenSource::Value(&identity.token),
        log_level: pool.spec.log_level,
        // Edit/Run only (CEL), both of which serve on 80.
        port: 80,
        origin: config
            .runner_hosts
            .first()
            .map(|host| format!("https://{host}")),
        command: pool.spec.command,
        python_runtime,
        cpu: pool.spec.cpu.clone(),
        memory: pool.spec.memory.clone(),
        env,
        env_from: None,
        mode: WorkspaceMode::Pooled,
        affinity: None,
        slot_volume: slot_volume::warm_slot_volume(
            pool.spec
                .storage
                .as_ref()
                .and_then(|storage| storage.to_bytes()),
            python_runtime,
            pool.spec.s3_secret_name.clone(),
        ),
        extra_volumes: vec![Volume {
            name: CLAIM_VOLUME_NAME.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(claim_secret_name(&identity.name)),
                optional: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }],
        sidecars: pool.spec.sidecars.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::{PoolSpec, RunnerCommand};

    fn config() -> Config {
        Config::test_default()
    }

    fn pool(spec: PoolSpec) -> Pool {
        let mut pool = Pool::new("editors", spec);
        pool.metadata.namespace = Some("default".into());
        pool.metadata.uid = Some("11111111-2222-3333-4444-555555555555".into());
        pool
    }

    fn warm_pod(spec: PoolSpec) -> (Pod, WarmPodIdentity) {
        let pool = pool(spec);
        let identity = mint_identity("editors");
        let pod = build_warm_pod(&config(), &pool, &identity).unwrap();
        (pod, identity)
    }

    /// A warm pod belongs to no workspace: no affinity to attract siblings, no
    /// runner/workspace labels a Service could select, an anonymous slot.
    #[test]
    fn warm_pods_are_anonymous() {
        let (pod, _) = warm_pod(PoolSpec::default());
        let spec = pod.spec.as_ref().unwrap();
        assert!(spec.affinity.is_none());
        let labels = pod.metadata.labels.as_ref().unwrap();
        assert!(!labels.contains_key("kubimo.aqora.io/name"));
        assert!(!labels.contains_key("kubimo.aqora.io/workspace"));
        assert_eq!(labels.get(POOL_LABEL).unwrap(), "editors");
        assert_eq!(labels.get(POOL_STATE_LABEL).unwrap(), POOL_STATE_WARM);
        let volume = &spec.volumes.as_ref().unwrap()[0];
        let attrs = volume.csi.as_ref().unwrap().volume_attributes.as_ref();
        assert_eq!(attrs.unwrap().get("pooled").unwrap(), "true");
        assert!(!attrs.unwrap().contains_key("workspace"));
    }

    /// The minted identity is baked into the command *and* recorded as
    /// annotations — the annotations are what the claim reads back, so the two
    /// must agree.
    #[test]
    fn minted_identity_matches_between_command_and_annotations() {
        let (pod, identity) = warm_pod(PoolSpec::default());
        let annotations = pod.metadata.annotations.as_ref().unwrap();
        assert_eq!(
            annotations.get(WARM_BASE_URL_ANNOTATION).unwrap(),
            &identity.base_url
        );
        assert_eq!(
            annotations.get(WARM_TOKEN_ANNOTATION).unwrap(),
            &identity.token
        );
        let command = pod.spec.as_ref().unwrap().containers[0]
            .command
            .as_ref()
            .unwrap();
        let arg_after = |flag: &str| {
            command
                .iter()
                .position(|arg| arg == flag)
                .map(|i| command[i + 1].as_str())
        };
        assert_eq!(arg_after("--base-url"), Some(identity.base_url.as_str()));
        assert_eq!(arg_after("--token"), Some(identity.token.as_str()));
        // The pre-boot switch: an env var, never a flag, so an older image
        // ignores it instead of crashing.
        let env = pod.spec.as_ref().unwrap().containers[0].env.as_ref();
        assert!(env.unwrap().iter().any(|var| {
            var.name == CLAIM_MARKER_ENV && var.value.as_deref() == Some("/home/me/.kubimo/claimed")
        }));
    }

    /// Re-minting must not change the template hash — it would retire every
    /// warm pod on every reconcile — while a template change must.
    #[test]
    fn template_hash_ignores_minted_identity_but_sees_spec_changes() {
        let config = config();
        let base = pool(PoolSpec::default());
        assert_eq!(template_hash(&config, &base), template_hash(&config, &base));

        let mut resized = pool(PoolSpec {
            replicas: 7,
            ..Default::default()
        });
        resized.spec.replicas = 7;
        assert_eq!(
            template_hash(&config, &base),
            template_hash(&config, &resized),
            "replicas is a sizing knob, not a pod shape"
        );

        let cpu = pool(PoolSpec {
            cpu: Some(kubimo::Requirement {
                min: Some("250m".parse().unwrap()),
                max: None,
            }),
            ..Default::default()
        });
        assert_ne!(template_hash(&config, &base), template_hash(&config, &cpu));

        let command = pool(PoolSpec {
            command: RunnerCommand::Run,
            ..Default::default()
        });
        assert_ne!(
            template_hash(&config, &base),
            template_hash(&config, &command)
        );
    }

    /// The shared asset origin is baked into a warm pod at boot as an env var
    /// (never a flag — an older image must ignore it, not crash), so flipping
    /// it must change the template hash and retire the fleet, while an
    /// upgrade with the feature off must leave both pod and hash untouched.
    #[test]
    fn asset_url_is_baked_into_env_and_template_hash_only_when_configured() {
        let asset_env = |pod: &Pod| {
            pod.spec.as_ref().unwrap().containers[0]
                .env
                .as_ref()
                .unwrap()
                .iter()
                .find(|var| var.name == "KUBIMO_ASSET_URL")
                .and_then(|var| var.value.clone())
        };

        let off = config();
        let (pod, _) = warm_pod(PoolSpec::default());
        assert_eq!(asset_env(&pod), None);

        let mut on = config();
        on.runner_asset_base_path = Some("/marimo-assets".into());
        let pod =
            build_warm_pod(&on, &pool(PoolSpec::default()), &mint_identity("editors")).unwrap();
        assert_eq!(
            asset_env(&pod),
            on.runner_asset_url(on.marimo_image(Default::default())),
        );

        let base = pool(PoolSpec::default());
        assert_ne!(template_hash(&off, &base), template_hash(&on, &base));
    }

    /// Sidecars read claim-time config from the per-pod Secret volume; the
    /// Secret is owned by the pod so it is collected with it.
    #[test]
    fn claim_secret_is_owned_by_the_pod() {
        let (mut pod, identity) = warm_pod(PoolSpec::default());
        pod.metadata.uid = Some("66666666-7777-8888-9999-000000000000".into());
        let secret = claim_secret(&pod).unwrap();
        assert_eq!(
            secret.metadata.name.as_deref(),
            Some(claim_secret_name(&identity.name).as_str())
        );
        let owner = &secret.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!(owner.kind, "Pod");
        assert_eq!(owner.name, identity.name);

        let volumes = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        assert!(volumes.iter().any(|volume| {
            volume.name == CLAIM_VOLUME_NAME
                && volume
                    .secret
                    .as_ref()
                    .and_then(|s| s.secret_name.as_deref())
                    == Some(&claim_secret_name(&identity.name)[..])
        }));
    }
}
