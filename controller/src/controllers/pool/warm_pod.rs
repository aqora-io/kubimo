//! Minting and building warm pods.
//!
//! A warm pod is a runner pod with no runner: same image, sandbox, probes and
//! start.sh contract, but booted on an anonymous slot with a base-url and
//! token minted here. Both are baked into the pod command — marimo cannot
//! change them once serving — and recorded as annotations, which is what the
//! runner reconciler reads back at claim time.
//!
//! Each warm pod also gets its own Service and Ingress at mint, so that
//! ingress-nginx has long programmed the route by the time the pod is claimed.
//! The Service selects a label the pod only gets once the claim is acked.

use std::collections::BTreeMap;

use kubimo::k8s_openapi::api::core::v1::{
    EnvVar, Pod, Secret, SecretVolumeSource, Service, ServicePort, ServiceSpec, Volume,
};
use kubimo::k8s_openapi::api::networking::v1::Ingress;
use kubimo::kube::api::ObjectMeta;
use kubimo::pool::{
    CLAIM_MARKER_ENV, CLAIM_MARKER_RELATIVE_PATH, MIGRATION_MARKER_ENV,
    MIGRATION_MARKER_RELATIVE_PATH, POOL_LABEL, POOL_STATE_LABEL, POOL_STATE_WARM,
    POOL_TEMPLATE_HASH_ANNOTATION, ROUTE_LABEL, WARM_BASE_URL_ANNOTATION, WARM_ROUTE_ANNOTATION,
    WARM_TOKEN_ANNOTATION,
};
use kubimo::{Pool, prelude::*};
use sha2::{Digest, Sha256};

use crate::Config;
use crate::context::Context;
use crate::controllers::ingress::{IngressParams, build_ingress, ingress_path_from_name};
use crate::controllers::runner_pod::{RunnerPodParams, TokenSource, build_runner_pod};
use crate::controllers::slot_volume;

/// The volume name pool sidecar templates mount the per-pod claim Secret by.
pub(crate) const CLAIM_VOLUME_NAME: &str = "claim";

/// Edit/Run only (CEL), both of which serve on 80.
const PORT: i32 = 80;

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

/// Bumped whenever this controller changes how it builds a warm pod beyond
/// what [`template_hash`] fingerprints from the pool, so pods minted by an
/// older build are retired rather than claimed.
const WARM_POD_SHAPE: u32 = 2;

/// Everything that decides what a warm pod *is*, hashed so drift can be
/// detected without diffing pod specs. Deliberately excludes the minted
/// name/token/base-url (random per pod) and `replicas` (a sizing knob, not a
/// shape).
pub(crate) fn template_hash(config: &Config, pool: &Pool) -> String {
    let image = &config.marimo_image;
    let mut fingerprint = serde_json::json!({
        "shape": WARM_POD_SHAPE,
        "image": image,
        "command": pool.spec.command,
        // Resolved, not as written: absent is Uv, so spelling the default out
        // must not retire warm pods that already boot uv.
        "pythonRuntime": pool.spec.python_runtime.unwrap_or_default(),
        "logLevel": pool.spec.log_level,
        "cpu": pool.spec.cpu,
        "memory": pool.spec.memory,
        "env": pool.spec.env,
        "sidecars": pool.spec.sidecars,
        "s3SecretName": pool.spec.s3_secret_name,
        "storage": pool.spec.storage,
        "origin": config.runner_hosts.first(),
        // What the Ingress minted with the pod is built from: it is never
        // re-applied, so a config change retires the pod instead.
        "ingress": {
            "className": config.ingress_class_name,
            "hosts": config.runner_hosts,
            "clusterIssuer": config.cluster_issuer,
            "proxyTimeoutSecs": config.runner_proxy_timeout_secs,
        },
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
    let image = config.marimo_image.clone();
    let mut env = pool.spec.env.clone().unwrap_or_default();
    env.push(EnvVar {
        name: CLAIM_MARKER_ENV.to_string(),
        value: Some(format!(
            "{dir}/{CLAIM_MARKER_RELATIVE_PATH}",
            dir = slot_volume::MOUNT_DIR
        )),
        ..Default::default()
    });
    env.push(EnvVar {
        name: MIGRATION_MARKER_ENV.to_string(),
        value: Some(format!(
            "{dir}/{MIGRATION_MARKER_RELATIVE_PATH}",
            dir = slot_volume::MOUNT_DIR
        )),
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
            (WARM_ROUTE_ANNOTATION.to_string(), identity.name.clone()),
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
        port: PORT,
        origin: config
            .runner_hosts
            .first()
            .map(|host| format!("https://{host}")),
        command: pool.spec.command,
        python_runtime: pool.spec.python_runtime.unwrap_or_default(),
        cpu: pool.spec.cpu.clone(),
        memory: pool.spec.memory.clone(),
        env,
        env_from: None,
        affinity: None,
        slot_volume: slot_volume::warm_slot_volume(
            pool.spec
                .storage
                .as_ref()
                .and_then(|storage| storage.to_bytes()),
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

/// The name of the Service and Ingress the pod is routed through, or `None`
/// for a pod minted before warm pods were routed from birth.
pub(crate) fn route_name(pod: &Pod) -> Option<&str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(WARM_ROUTE_ANNOTATION)
        .map(String::as_str)
}

fn require_route_name(pod: &Pod) -> kubimo::Result<&str> {
    route_name(pod).ok_or(kubimo::Error::ObjectMetaMissing(WARM_ROUTE_ANNOTATION))
}

/// The warm pod's own Service. It selects only [`ROUTE_LABEL`], which the pod
/// is minted without, so it has no endpoints until the claim is acked.
pub(crate) fn route_service(pod: &Pod) -> kubimo::Result<Service> {
    let name = require_route_name(pod)?;
    Ok(Service {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: pod.metadata.namespace.clone(),
            owner_references: Some(vec![pod.static_controller_owner_ref()?]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(
                ROUTE_LABEL.to_string(),
                name.to_string(),
            )])),
            ports: Some(vec![ServicePort {
                name: Some("marimo".to_string()),
                port: PORT,
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// The warm pod's own Ingress: the minted base-url, routed the way a cold
/// runner with default ingress settings is, but to the Service's ClusterIP.
/// ingress-nginx applies endpoint changes in a config sync, at most one every
/// ~3.3 s by default, and the claim's replacement warm pod has usually just
/// taken one with its new Ingress. The ClusterIP is programmed at mint, so the
/// ack's label only has to reach kube-proxy.
pub(crate) fn route_ingress(config: &Config, pod: &Pod) -> kubimo::Result<Ingress> {
    let name = require_route_name(pod)?;
    let path = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(WARM_BASE_URL_ANNOTATION))
        .ok_or(kubimo::Error::ObjectMetaMissing(WARM_BASE_URL_ANNOTATION))?;
    let mut ingress = build_ingress(
        config,
        IngressParams {
            name,
            namespace: pod.metadata.namespace.clone(),
            owner_reference: pod.static_controller_owner_ref()?,
            path: path.clone(),
            service_name: name,
            port: PORT,
            ingress: None,
        },
    );
    ingress.metadata.annotations.get_or_insert_default().insert(
        "nginx.ingress.kubernetes.io/service-upstream".to_string(),
        "true".to_string(),
    );
    Ok(ingress)
}

/// Create the pod's Service and Ingress where missing. An existing one is
/// never re-applied: ingress-nginx's validating webhook tests the whole nginx
/// config on every Ingress write, no-op or not.
pub(crate) async fn ensure_route(ctx: &Context, pod: &Pod) -> kubimo::Result<()> {
    let namespace = pod.require_namespace()?;
    let name = require_route_name(pod)?;
    // The Service first: ingress-nginx has no ClusterIP to proxy to before.
    let services = ctx.api_namespaced::<Service>(namespace);
    if services.get_opt(name).await?.is_none() {
        services.patch(&route_service(pod)?).await?;
    }
    let ingresses = ctx.api_namespaced::<Ingress>(namespace);
    if ingresses.get_opt(name).await?.is_none() {
        ingresses.patch(&route_ingress(&ctx.config, pod)?).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::ingress::{IngressParams, build_ingress};
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

    fn sandbox_env(pod: &Pod) -> Vec<Option<String>> {
        pod.spec.as_ref().unwrap().containers[0]
            .env
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|var| var.name == crate::controllers::runner_pod::SANDBOX_ENV)
            .map(|var| var.value.clone())
            .collect()
    }

    /// marimo boots with the pool's backend before any workspace claims the
    /// pod; the pool's own env cannot override it.
    #[test]
    fn a_warm_pod_boots_its_pools_sandbox_backend() {
        use kubimo::WorkspacePythonRuntime::{Conda, Uv};
        use kubimo::k8s_openapi::api::core::v1::EnvVar;
        for (python_runtime, backend) in [(None, "uv"), (Some(Uv), "uv"), (Some(Conda), "pixi")] {
            let (pod, _) = warm_pod(PoolSpec {
                python_runtime,
                env: Some(vec![EnvVar {
                    name: crate::controllers::runner_pod::SANDBOX_ENV.into(),
                    value: Some("bogus".into()),
                    ..Default::default()
                }]),
                ..Default::default()
            });
            assert_eq!(
                sandbox_env(&pod),
                vec![Some(backend.to_string())],
                "{python_runtime:?}"
            );
        }
    }

    /// A pool recreated under the same name with the other runtime must not
    /// claim the pods minted for the first; naming the default changes nothing.
    #[test]
    fn the_template_hash_follows_the_resolved_runtime() {
        use kubimo::WorkspacePythonRuntime::{Conda, Uv};
        let hash = |python_runtime| {
            template_hash(
                &config(),
                &pool(PoolSpec {
                    python_runtime,
                    ..Default::default()
                }),
            )
        };
        assert_eq!(hash(None), hash(Some(Uv)));
        assert_ne!(hash(None), hash(Some(Conda)));
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
        // Where the pod reports the migration the claim starts, outside the
        // root-owned directory of the claim marker.
        assert!(env.unwrap().iter().any(|var| {
            var.name == MIGRATION_MARKER_ENV
                && var.value.as_deref() == Some("/home/me/.kubimo-migration")
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
        assert_eq!(asset_env(&pod), on.runner_asset_url(&on.marimo_image));

        let base = pool(PoolSpec::default());
        assert_ne!(template_hash(&off, &base), template_hash(&on, &base));
    }

    /// A warm pod is routed from birth by its own Service and Ingress, but the
    /// Service selects a label the pod is minted without, so nothing reaches
    /// the pod before the claim is acked. Both are owned by the pod.
    #[test]
    fn a_warm_pods_service_selects_it_only_once_routed() {
        let (mut pod, identity) = warm_pod(PoolSpec::default());
        pod.metadata.uid = Some("66666666-7777-8888-9999-000000000000".into());
        assert_eq!(route_name(&pod), Some(identity.name.as_str()));
        assert!(
            !pod.metadata
                .labels
                .as_ref()
                .unwrap()
                .contains_key(ROUTE_LABEL)
        );

        let service = route_service(&pod).unwrap();
        assert_eq!(
            service.metadata.name.as_deref(),
            Some(identity.name.as_str())
        );
        let owner = &service.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!(
            (owner.kind.as_str(), owner.name.as_str()),
            ("Pod", identity.name.as_str())
        );
        let spec = service.spec.as_ref().unwrap();
        assert_eq!(
            spec.selector.as_ref().unwrap(),
            &BTreeMap::from([(ROUTE_LABEL.to_string(), identity.name.clone())])
        );
        assert_eq!(spec.ports.as_ref().unwrap()[0].port, 80);
    }

    /// The Ingress is a cold runner's Ingress built from the configured
    /// defaults, pointed at the route Service, plus `service-upstream`: nginx
    /// proxies to the ClusterIP it was programmed with at mint, so the ack's
    /// label reaches the pod through kube-proxy, not an ingress-nginx sync.
    #[test]
    fn a_warm_pods_ingress_routes_its_base_url_through_the_service_cluster_ip() {
        let mut config = config();
        config.runner_hosts = vec!["kubimo.org".into()];
        let (mut pod, identity) = warm_pod(PoolSpec::default());
        pod.metadata.uid = Some("66666666-7777-8888-9999-000000000000".into());

        let ingress = route_ingress(&config, &pod).unwrap();
        let mut expected = build_ingress(
            &config,
            IngressParams {
                name: &identity.name,
                namespace: Some("default".into()),
                owner_reference: pod.static_controller_owner_ref().unwrap(),
                path: identity.base_url.clone(),
                service_name: &identity.name,
                port: 80,
                ingress: None,
            },
        );
        expected.metadata.annotations.as_mut().unwrap().insert(
            "nginx.ingress.kubernetes.io/service-upstream".into(),
            "true".into(),
        );
        assert_eq!(ingress, expected);
    }

    /// A pod minted by an older controller has no route of its own; its
    /// runner keeps routing it through the runner's Service and Ingress.
    #[test]
    fn a_pod_without_the_route_marker_has_no_route() {
        assert_eq!(route_name(&Pod::default()), None);
        assert!(route_service(&Pod::default()).is_err());
    }

    /// The minted Ingress is never re-applied, so a warm pod whose Ingress no
    /// longer matches the ingress config must be retired instead.
    #[test]
    fn the_template_hash_follows_the_ingress_config() {
        let base = pool(PoolSpec::default());
        let hash = |config: &Config| template_hash(config, &base);
        let default = hash(&config());
        let changes: [fn(&mut Config); 4] = [
            |c| c.ingress_class_name = "traefik".into(),
            |c| c.runner_hosts = vec!["kubimo.org".into(), "other.kubimo.org".into()],
            |c| c.cluster_issuer = Some("letsencrypt".into()),
            |c| c.runner_proxy_timeout_secs = 60,
        ];
        for change in changes {
            let mut changed = config();
            change(&mut changed);
            assert_ne!(hash(&changed), default);
        }
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
