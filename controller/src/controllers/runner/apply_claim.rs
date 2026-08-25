//! Claiming a pre-booted warm pod instead of cold-starting one.
//!
//! Best effort by design: every ineligibility, empty pool or lost race falls
//! back to the ordinary cold path, so `spec.pool` can never make a runner
//! *worse* than it would have been without it. The one thing that must never
//! happen is a half-claim — a pod that two runners both believe is theirs, or
//! one the pool retires mid-claim — which is why the state transition is a
//! JSON-Patch guarded by a `test` on the previous state label.

use std::collections::BTreeMap;

use kubimo::k8s_openapi::ByteString;
use kubimo::k8s_openapi::api::core::v1::{Container, Pod, Secret};
use kubimo::pool::{
    CLAIM_ANNOTATION, CLAIM_STATE_ANNOTATION, CLAIM_STATE_BOUND, CLAIM_STATE_FAILED, POOL_LABEL,
    POOL_STATE_CLAIMED, POOL_STATE_LABEL, POOL_STATE_WARM, PoolClaim, WARM_BASE_URL_ANNOTATION,
    WARM_TOKEN_ANNOTATION,
};
use kubimo::{
    CpuQuantity, CpuUnit, FilterParams, KubimoLabel, Pool, Requirement, Runner, RunnerClaim,
    StorageQuantity, Workspace, WorkspacePythonRuntime, json_patch_macros::*, prelude::*,
};

use crate::context::Context;
use crate::controllers::slot_volume::SlotSources;
use crate::controllers::workspace_affinity;

use super::RunnerReconciler;

pub(crate) enum ClaimOutcome {
    /// A warm pod is bound (or binding) to this runner. Until `acked`, the
    /// agent is still linking and hydrating the slot, and the Service/Ingress
    /// must not exist yet — routing users to an unhydrated workspace is the
    /// claim's one unacceptable failure mode.
    Claimed { acked: bool },
    /// Build the pod the ordinary way.
    ColdPath,
}

impl RunnerReconciler {
    pub(crate) async fn apply_claim(
        &self,
        ctx: &Context,
        runner: &Runner,
        workspace: &Workspace,
        python_runtime: WorkspacePythonRuntime,
    ) -> Result<ClaimOutcome, kubimo::Error> {
        let Some(pool_name) = runner.spec.pool.as_deref() else {
            return Ok(ClaimOutcome::ColdPath);
        };
        let namespace = runner.require_namespace()?;
        let runner_name = runner.name()?;
        let pods = ctx.api_namespaced::<Pod>(namespace);

        // The pod is the source of truth; status is a cache. A cold pod named
        // after the runner means the decision was already made the other way —
        // claiming now would strand it.
        if pods.get_opt(runner_name).await?.is_some() {
            return Ok(ClaimOutcome::ColdPath);
        }

        // Already claimed? The claim patch writes the runner's identity onto
        // the pod, so a reconcile that crashed between the patch and the
        // status write finds its pod here instead of claiming a second one.
        let name_label = KubimoLabel::borrow("name").to_string();
        let mine: Vec<Pod> = list_pods(
            &pods,
            &FilterParams::new().with_labels((name_label.as_str(), runner_name)),
        )
        .await?;
        if let Some(pod) = mine
            .into_iter()
            .find(|pod| has_label(pod, POOL_LABEL) && pod.metadata.deletion_timestamp.is_none())
        {
            return self.adopt_claimed_pod(ctx, runner, pool_name, pod).await;
        }

        // Eligibility. Every miss is a cold start, logged so "why didn't it
        // claim" is answerable from the controller log.
        let Some(pool) = ctx
            .api_namespaced::<Pool>(namespace)
            .get_opt(pool_name)
            .await?
        else {
            return cold(runner_name, pool_name, "pool does not exist");
        };
        if let Err(reason) = eligible(ctx, runner, workspace, &pool, python_runtime) {
            return cold(runner_name, pool_name, reason);
        }
        // One workspace, one slot: any live pod of this workspace (even one
        // still terminating) is bound to a specific node's slot, and a claim
        // lands wherever its warm pod happens to be. Two pods on two nodes
        // would give one workspace two diverging copies.
        //
        // Pods in a terminal phase do not count: a finished cache job's pod
        // lingers as Succeeded/Failed until its Job is collected, holds no
        // mount, and the platform runs one against every fresh workspace —
        // counting those would block claims for exactly the workspaces the
        // pool exists to serve.
        let mut workspace_pods = list_pods(
            &pods,
            &FilterParams::new()
                .with_labels(workspace_affinity::workspace_label(&runner.spec.workspace)),
        )
        .await?;
        workspace_pods.retain(may_hold_a_slot);
        if !workspace_pods.is_empty() {
            return cold(
                runner_name,
                pool_name,
                "the workspace already has runner pods",
            );
        }

        let mut warm: Vec<Pod> = list_pods(
            &pods,
            &FilterParams::new().with_labels(
                [(POOL_LABEL, pool_name), (POOL_STATE_LABEL, POOL_STATE_WARM)]
                    .into_iter()
                    .collect::<kubimo::Selector>(),
            ),
        )
        .await?;
        warm.retain(|pod| pod.metadata.deletion_timestamp.is_none());
        // Oldest first: most likely to be fully booted.
        warm.sort_by_key(|pod| pod.metadata.creation_timestamp.clone());

        let claim = pool_claim(runner, workspace);
        let claim_json = serde_json::to_string(&claim)?;
        for pod in &warm {
            let pod_name = pod.name()?;
            let patched = pods
                .patch_json(
                    pod_name,
                    patch![
                        // Atomicity: a concurrent claim or retire flips this
                        // label first, and the whole patch fails with a 422.
                        test!(["metadata", "labels", POOL_STATE_LABEL] => POOL_STATE_WARM),
                        put!(["metadata", "labels", POOL_STATE_LABEL] => POOL_STATE_CLAIMED),
                        add!(["metadata", "labels", name_label.as_str()] => runner_name),
                        add!(["metadata", "labels", workspace_affinity::workspace_label(&runner.spec.workspace).0.as_str()]
                            => runner.spec.workspace),
                        // The pod leaves the pool's ownership for the
                        // runner's: it now lives and dies with the Runner.
                        put!(["metadata", "ownerReferences"] => vec![runner.static_controller_owner_ref()?]),
                        add!(["metadata", "annotations", CLAIM_ANNOTATION] => claim_json),
                    ],
                )
                .await;
            match patched {
                Ok(pod) => {
                    tracing::info!(
                        runner = runner_name,
                        pool = pool_name,
                        pod = pod_name,
                        "claimed a warm pod"
                    );
                    return self.adopt_claimed_pod(ctx, runner, pool_name, pod).await;
                }
                // Lost the race for this pod; try the next.
                Err(err) if super::is_invalid_request(&err) => continue,
                Err(kubimo::Error::Kube(kubimo::kube::Error::Api(status)))
                    if status.code == 404 || status.code == 409 =>
                {
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        cold(runner_name, pool_name, "no warm pods available")
    }

    /// Converge on a pod this runner has already claimed: heal `status.claim`,
    /// keep the sidecar Secret filled, and turn a failed claim back into a
    /// cold start.
    async fn adopt_claimed_pod(
        &self,
        ctx: &Context,
        runner: &Runner,
        pool_name: &str,
        pod: Pod,
    ) -> Result<ClaimOutcome, kubimo::Error> {
        let namespace = runner.require_namespace()?;
        let annotations = pod.metadata.annotations.clone().unwrap_or_default();
        if annotations.get(CLAIM_STATE_ANNOTATION).map(String::as_str) == Some(CLAIM_STATE_FAILED) {
            // The agent could not bind the slot (no anonymous slot after an
            // agent replacement, lost credentials, hydration failure). The
            // pod is unusable — its slot may be half-hydrated — so delete it
            // and fall back cold; the pool mints a replacement.
            tracing::warn!(
                runner = runner.name()?,
                pod = pod.name()?,
                error = annotations
                    .get(kubimo::pool::CLAIM_ERROR_ANNOTATION)
                    .map(String::as_str)
                    .unwrap_or("unknown"),
                "claim failed; deleting the pod and falling back to a cold start"
            );
            ctx.api_namespaced::<Pod>(namespace)
                .delete_opt(pod.name()?)
                .await?;
            self.record_claim(ctx, runner, None).await?;
            return Ok(ClaimOutcome::ColdPath);
        }

        // Two runners of one workspace can race the zero-pod eligibility check
        // and each claim a warm pod — on two nodes, two diverging copies of
        // the workspace's slot, the one thing pooled mode must never produce.
        // Every reconcile re-checks here with the same deterministic rule
        // (oldest claimed pod, name as tie-break), so whichever racer observes
        // its rival and loses concedes: it deletes its own pod and cold-starts
        // against the survivor, whose node the cold pod's workspace affinity
        // then targets. Observations can differ for a moment, but concessions
        // only ever shrink the set, and the requeue loop re-runs this until
        // one pod remains.
        let pods = ctx.api_namespaced::<Pod>(namespace);
        let mut claimed = list_pods(
            &pods,
            &FilterParams::new()
                .with_labels(workspace_affinity::workspace_label(&runner.spec.workspace)),
        )
        .await?;
        claimed.retain(|other| {
            has_label(other, POOL_LABEL)
                && other.metadata.deletion_timestamp.is_none()
                && may_hold_a_slot(other)
        });
        if let Some(winner) = claim_winner(&claimed)
            && winner != pod.name()?
        {
            tracing::warn!(
                runner = runner.name()?,
                pod = pod.name()?,
                winner,
                "another claimed pod already holds this workspace; conceding to it"
            );
            pods.delete_opt(pod.name()?).await?;
            // No status write needed: the ColdPath arm in the reconciler
            // clears any claim an earlier reconcile recorded.
            return Ok(ClaimOutcome::ColdPath);
        }

        let acked =
            annotations.get(CLAIM_STATE_ANNOTATION).map(String::as_str) == Some(CLAIM_STATE_BOUND);
        let claim = RunnerClaim {
            pool: pool_name.to_string(),
            pod_name: pod.name()?.to_string(),
            // Absent annotations would mean a pod that was never a warm pod;
            // the empty path fails loudly downstream (ingress rejects it)
            // rather than silently routing to the wrong place.
            ingress_path: annotations
                .get(WARM_BASE_URL_ANNOTATION)
                .cloned()
                .unwrap_or_default(),
            token: annotations.get(WARM_TOKEN_ANNOTATION).cloned(),
        };
        if runner.status.as_ref().and_then(|s| s.claim.as_ref()) != Some(&claim) {
            self.record_claim(ctx, runner, Some(claim)).await?;
        }
        // Ack or no ack: the ack only means the slot is hydrated, not that
        // the sidecar Secret was ever filled. Gated on `!acked`, a controller
        // that crashed — or hit Secrets the platform had not created yet —
        // before one copy succeeded would never copy them at all once the
        // agent acked, and a sidecar reading the claim Secret would wait on
        // it forever.
        self.copy_sidecar_secrets(ctx, runner, &pod).await?;
        Ok(ClaimOutcome::Claimed { acked })
    }

    /// Drop a recorded claim whose pod is gone, returning whether one was
    /// dropped. A cold start makes `spec` the routing truth again, and every
    /// consumer — the platform, `effective_ingress_path`, the status loop —
    /// prefers `status.claim` when present: left behind, a stale claim keeps
    /// them all pointed at the dead pod's base-url/token forever, while the
    /// cold pod serves the spec's.
    pub(crate) async fn clear_stale_claim(
        &self,
        ctx: &Context,
        runner: &Runner,
    ) -> Result<bool, kubimo::Error> {
        if runner
            .status
            .as_ref()
            .and_then(|status| status.claim.as_ref())
            .is_none()
        {
            return Ok(false);
        }
        self.record_claim(ctx, runner, None).await?;
        Ok(true)
    }

    async fn record_claim(
        &self,
        ctx: &Context,
        runner: &Runner,
        claim: Option<RunnerClaim>,
    ) -> Result<(), kubimo::Error> {
        let namespace = runner.require_namespace()?;
        // Clone-and-augment rather than a fresh status: this apply carries the
        // whole status under the shared field manager, so starting from
        // Default would blank the conditions the status loop just wrote.
        let mut patched = runner.clone();
        let mut status = runner.status.clone().unwrap_or_default();
        status.claim = claim;
        patched.status = Some(status);
        ctx.api_namespaced::<Runner>(namespace)
            .patch_status(&patched)
            .await?;
        Ok(())
    }

    /// Copy the Secrets the runner's sidecars reference into the claimed
    /// pod's `<pod>-claim` Secret, which is what the pool's sidecar template
    /// actually mounts. A warm pod's sidecars were created before the runner
    /// existed, so they cannot reference the runner's own Secrets; the pool
    /// template reads from the mounted claim volume instead, and kubelet
    /// propagates this update into it within seconds.
    async fn copy_sidecar_secrets(
        &self,
        ctx: &Context,
        runner: &Runner,
        pod: &Pod,
    ) -> Result<(), kubimo::Error> {
        let names = sidecar_secret_names(runner.spec.sidecars.as_deref().unwrap_or_default());
        if names.is_empty() {
            return Ok(());
        }
        let namespace = runner.require_namespace()?;
        let secrets = ctx.api_namespaced::<Secret>(namespace);
        let mut data: BTreeMap<String, ByteString> = BTreeMap::new();
        for name in names {
            // Missing referenced Secrets propagate as errors: on the cold
            // path kubelet would hold the sidecar back the same way, and the
            // backoff retries until the platform finishes creating them.
            let secret = secrets.get(&name).await?;
            for (key, value) in secret.data.unwrap_or_default() {
                // The claim Secret flattens every referenced Secret into one
                // volume; a shared key would silently hand one sidecar the
                // other's credential, so refuse loudly instead.
                if data.insert(key.clone(), value).is_some() {
                    return Err(kubimo::Error::Custom(format!(
                        "sidecar Secrets of runner {runner} share the key {key:?}; \
                         cannot flatten them into the claim Secret",
                        runner = runner.name()?,
                    )));
                }
            }
        }
        let mut claim_secret = crate::controllers::pool::warm_pod::claim_secret(pod)?;
        claim_secret.data = Some(data);
        // Re-applying under the same manager must restate the ownerReference,
        // or this apply would relinquish it and orphan the Secret from the
        // pod's garbage-collection chain.
        secrets.patch(&claim_secret).await?;
        Ok(())
    }
}

fn cold(runner: &str, pool: &str, reason: &str) -> Result<ClaimOutcome, kubimo::Error> {
    tracing::info!(runner, pool, reason, "not claiming; taking the cold path");
    Ok(ClaimOutcome::ColdPath)
}

async fn list_pods(
    pods: &kubimo::Api<Pod>,
    params: &FilterParams,
) -> Result<Vec<Pod>, kubimo::Error> {
    use futures::TryStreamExt;
    pods.list(params)
        .map_ok(|item| item.item)
        .try_collect()
        .await
}

/// Whether a workspace-labelled pod can still be holding the workspace's
/// slot. Terminal pods cannot: their volumes are unpublished, and kubelet
/// never republishes a Succeeded/Failed pod. Everything else — running,
/// pending, terminating, unknown — must count.
fn may_hold_a_slot(pod: &Pod) -> bool {
    !matches!(
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref()),
        Some("Succeeded" | "Failed")
    )
}

/// The one claimed pod of a workspace every observer agrees should survive a
/// double-claim: the oldest, by name on a timestamp tie. Racers may see
/// different subsets for a moment, but any pod whose owner observes a rival
/// and loses this comparison is deleted, so the set only ever shrinks toward
/// a single survivor.
fn claim_winner(claimed: &[Pod]) -> Option<&str> {
    claimed
        .iter()
        .min_by_key(|pod| {
            (
                pod.metadata.creation_timestamp.clone(),
                pod.metadata.name.clone(),
            )
        })
        .and_then(|pod| pod.metadata.name.as_deref())
}

fn has_label(pod: &Pod, key: &str) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .is_some_and(|labels| labels.contains_key(key))
}

/// The claim payload the agent executes: the workspace identity plus its slot
/// sources, the same values the cold path would have put in the volume
/// attributes.
fn pool_claim(runner: &Runner, workspace: &Workspace) -> PoolClaim {
    let sources = SlotSources::from_workspace(Some(workspace));
    let (bucket, key_prefix) = sources.archive.unwrap_or_default();
    let (seed_bucket, seed_key_prefix, seed_secrets) = match sources.seed {
        Some((bucket, key_prefix, secrets)) => (Some(bucket), key_prefix, Some(secrets)),
        None => (None, None, None),
    };
    PoolClaim {
        workspace: runner.spec.workspace.clone(),
        bucket,
        key_prefix,
        seed_bucket,
        seed_key_prefix,
        seed_secrets,
        limit_bytes: sources.limit_bytes,
    }
}

/// Whether this runner may claim from this pool. Everything checked here is
/// immutable on a running pod — the claim can change labels and annotations,
/// nothing else.
fn eligible(
    ctx: &Context,
    runner: &Runner,
    workspace: &Workspace,
    pool: &Pool,
    python_runtime: WorkspacePythonRuntime,
) -> Result<(), &'static str> {
    if pool.spec.command != runner.spec.command {
        return Err("command differs from the pool's");
    }
    if pool.spec.python_runtime.unwrap_or_default() != python_runtime {
        return Err("python runtime differs from the pool's");
    }
    let sources = SlotSources::from_workspace(Some(workspace));
    // Load-bearing, not cosmetic: kubelet handed the agent the pool's S3
    // secret at NodePublishVolume, long before this claim. If the workspace's
    // archive lives under different credentials, hydration would fail (or
    // worse, write somewhere unexpected).
    if sources.credentials_secret != pool.spec.s3_secret_name {
        return Err("workspace S3 credentials secret differs from the pool's");
    }
    if !storage_requirements_match(runner.spec.memory.as_ref(), pool.spec.memory.as_ref()) {
        return Err("memory requirements differ from the pool's");
    }
    if !cpu_requirements_match(runner.spec.cpu.as_ref(), pool.spec.cpu.as_ref()) {
        return Err("cpu requirements differ from the pool's");
    }
    // The slot is re-quota'd to the workspace's max at claim time; a max below
    // the pool's interim quota could already be exceeded by the venv template
    // and would fail hydration in a loop.
    let pool_quota = pool
        .spec
        .storage
        .as_ref()
        .and_then(StorageQuantity::to_bytes);
    if let (Some(limit), Some(pool_quota)) = (sources.limit_bytes, pool_quota)
        && limit < pool_quota
    {
        return Err("workspace storage max is below the pool's slot quota");
    }
    // Env is immutable on a running pod: everything the runner wants must
    // already be baked into the pool template.
    let pool_env = pool.spec.env.as_deref().unwrap_or_default();
    if !runner
        .spec
        .env
        .as_deref()
        .unwrap_or_default()
        .iter()
        .all(|var| pool_env.contains(var))
    {
        return Err("env is not a subset of the pool's");
    }
    if runner
        .spec
        .env_from
        .as_deref()
        .is_some_and(|e| !e.is_empty())
    {
        return Err("envFrom cannot be honoured on a claimed pod");
    }
    if !sidecars_match(
        runner.spec.sidecars.as_deref().unwrap_or_default(),
        pool.spec.sidecars.as_deref().unwrap_or_default(),
    ) {
        return Err("sidecars differ from the pool template's");
    }
    if let Some(log_level) = runner.spec.log_level
        && pool.spec.log_level != Some(log_level)
    {
        return Err("log level differs from the pool's");
    }
    // `--origin` was baked at boot from the configured host; a runner that
    // asks for a different first host would get the wrong allowed origin.
    let spec_host = runner
        .spec
        .ingress
        .as_ref()
        .and_then(|ingress| ingress.tls.as_ref())
        .and_then(|tls| tls.hosts.as_ref())
        .and_then(|hosts| hosts.first());
    if let Some(host) = spec_host
        && ctx.config.runner_hosts.first() != Some(host)
    {
        return Err("ingress host differs from the configured runner host");
    }
    Ok(())
}

fn storage_requirements_match(
    a: Option<&Requirement<StorageQuantity>>,
    b: Option<&Requirement<StorageQuantity>>,
) -> bool {
    fn norm(req: Option<&Requirement<StorageQuantity>>) -> (Option<u64>, Option<u64>) {
        (
            req.and_then(|r| r.min.as_ref())
                .and_then(StorageQuantity::to_bytes),
            req.and_then(|r| r.max.as_ref())
                .and_then(StorageQuantity::to_bytes),
        )
    }
    norm(a) == norm(b)
}

fn cpu_requirements_match(
    a: Option<&Requirement<CpuQuantity>>,
    b: Option<&Requirement<CpuQuantity>>,
) -> bool {
    fn norm(req: Option<&Requirement<CpuQuantity>>) -> (Option<u64>, Option<u64>) {
        (
            req.and_then(|r| r.min.as_ref()).and_then(cpu_millis),
            req.and_then(|r| r.max.as_ref()).and_then(cpu_millis),
        )
    }
    norm(a) == norm(b)
}

/// Normalize a CPU quantity to millicores so "1" and "1000m" compare equal.
fn cpu_millis(quantity: &CpuQuantity) -> Option<u64> {
    let repr = quantity.to_string();
    let repr = repr.trim();
    // A bare magnitude is cores.
    if let Ok(value) = repr.parse::<f64>() {
        return millis(value * 1000.0);
    }
    let split = repr.find(|c: char| c.is_ascii_alphabetic())?;
    let value: f64 = repr[..split].parse().ok()?;
    match repr[split..].parse::<CpuUnit>().ok()? {
        CpuUnit::Core => millis(value * 1000.0),
        CpuUnit::Milli => millis(value),
    }
}

fn millis(value: f64) -> Option<u64> {
    (value.is_finite() && value >= 0.0 && value < 2f64.powi(64)).then_some(value as u64)
}

/// Sidecars match when they are the same containers by (name, image),
/// order-independent. Deliberately not a deep compare: the pool template's
/// sidecars *replace* the runner's — they read their per-runner configuration
/// from the mounted claim Secret instead of the env the runner spec declares,
/// so their env/volumeMounts legitimately differ.
fn sidecars_match(runner: &[Container], pool: &[Container]) -> bool {
    fn keys(containers: &[Container]) -> Vec<(&str, Option<&str>)> {
        let mut keys: Vec<_> = containers
            .iter()
            .map(|container| (container.name.as_str(), container.image.as_deref()))
            .collect();
        keys.sort();
        keys
    }
    keys(runner) == keys(pool)
}

/// Every Secret named by the sidecars' env `secretKeyRef`s and `envFrom`
/// `secretRef`s — the per-runner material (api keys) the pool template's
/// sidecars need delivered through the claim Secret.
fn sidecar_secret_names(sidecars: &[Container]) -> Vec<String> {
    let mut names = Vec::new();
    for container in sidecars {
        for var in container.env.as_deref().unwrap_or_default() {
            if let Some(secret_ref) = var
                .value_from
                .as_ref()
                .and_then(|source| source.secret_key_ref.as_ref())
            {
                names.push(secret_ref.name.clone());
            }
        }
        for source in container.env_from.as_deref().unwrap_or_default() {
            if let Some(secret_ref) = source.secret_ref.as_ref() {
                names.push(secret_ref.name.clone());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::api::core::v1::{
        EnvFromSource, EnvVar, EnvVarSource, SecretEnvSource, SecretKeySelector,
    };

    /// The claim patch's opening `test` is the whole mutual-exclusion story;
    /// the ownerReference replacement is what moves the pod's lifetime from
    /// the Pool to the Runner.
    #[test]
    fn claim_patch_shape() {
        let patch = patch![
            test!(["metadata", "labels", POOL_STATE_LABEL] => POOL_STATE_WARM),
            put!(["metadata", "labels", POOL_STATE_LABEL] => POOL_STATE_CLAIMED),
            add!(["metadata", "labels", "kubimo.aqora.io/name"] => "bmor-x"),
            add!(["metadata", "labels", "kubimo.aqora.io/workspace"] => "bmow-x"),
            put!(["metadata", "ownerReferences"] => Vec::<()>::new()),
            add!(["metadata", "annotations", CLAIM_ANNOTATION] => "{}"),
        ];
        let json = serde_json::to_value(&patch).unwrap();
        assert_eq!(json[0]["op"], "test");
        assert_eq!(
            json[0]["path"],
            "/metadata/labels/kubimo.aqora.io~1pool-state"
        );
        assert_eq!(json[0]["value"], POOL_STATE_WARM);
        assert_eq!(json[4]["op"], "replace");
        assert_eq!(json[4]["path"], "/metadata/ownerReferences");
    }

    #[test]
    fn cpu_spellings_compare_equal() {
        let one: CpuQuantity = "1".parse().unwrap();
        let thousand_m: CpuQuantity = "1000m".parse().unwrap();
        let quarter: CpuQuantity = "250m".parse().unwrap();
        assert_eq!(cpu_millis(&one), Some(1000));
        assert_eq!(cpu_millis(&one), cpu_millis(&thousand_m));
        assert_eq!(cpu_millis(&quarter), Some(250));

        let req = |min: &str, max: &str| {
            Some(Requirement::<CpuQuantity> {
                min: Some(min.parse().unwrap()),
                max: Some(max.parse().unwrap()),
            })
        };
        assert!(cpu_requirements_match(
            req("250m", "2").as_ref(),
            req("250m", "2000m").as_ref()
        ));
        assert!(!cpu_requirements_match(
            req("250m", "2").as_ref(),
            req("500m", "2").as_ref()
        ));
        assert!(cpu_requirements_match(None, None));
        assert!(!cpu_requirements_match(req("250m", "2").as_ref(), None));
    }

    #[test]
    fn storage_spellings_compare_equal() {
        let req = |min: &str, max: &str| {
            Some(Requirement::<StorageQuantity> {
                min: Some(min.parse().unwrap()),
                max: Some(max.parse().unwrap()),
            })
        };
        assert!(storage_requirements_match(
            req("512Mi", "2Gi").as_ref(),
            req("512Mi", "2048Mi").as_ref()
        ));
        assert!(!storage_requirements_match(
            req("512Mi", "2Gi").as_ref(),
            req("512Mi", "4Gi").as_ref()
        ));
    }

    /// (name, image) is the identity; env and mounts legitimately differ
    /// because the pool template reads per-runner config from the claim
    /// Secret rather than the spec's env.
    #[test]
    fn sidecars_match_on_name_and_image_only() {
        let sidecar = |name: &str, image: &str, env: Option<Vec<EnvVar>>| Container {
            name: name.into(),
            image: Some(image.into()),
            env,
            ..Default::default()
        };
        let with_env = vec![sidecar(
            "api-proxy",
            "nginx:1.27-alpine",
            Some(vec![EnvVar {
                name: "API_KEY".into(),
                ..Default::default()
            }]),
        )];
        let without_env = vec![sidecar("api-proxy", "nginx:1.27-alpine", None)];
        assert!(sidecars_match(&with_env, &without_env));
        assert!(!sidecars_match(
            &with_env,
            &[sidecar("api-proxy", "nginx:1.28-alpine", None)]
        ));
        assert!(!sidecars_match(&with_env, &[]));
    }

    /// A finished cache job's pod lingers as Succeeded (or Failed) until its
    /// Job is collected, still wearing the workspace label — and the platform
    /// runs one against every fresh workspace. Counting it would block claims
    /// for exactly the workspaces the pool exists to serve.
    #[test]
    fn terminal_pods_do_not_block_a_claim() {
        let pod_in_phase = |phase: Option<&str>| Pod {
            status: Some(kubimo::k8s_openapi::api::core::v1::PodStatus {
                phase: phase.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!may_hold_a_slot(&pod_in_phase(Some("Succeeded"))));
        assert!(!may_hold_a_slot(&pod_in_phase(Some("Failed"))));
        assert!(may_hold_a_slot(&pod_in_phase(Some("Running"))));
        assert!(may_hold_a_slot(&pod_in_phase(Some("Pending"))));
        // No status at all: assume the worst.
        assert!(may_hold_a_slot(&Pod::default()));
    }

    /// Every observer of a double-claim must elect the same survivor, whatever
    /// subset it sees: oldest first, name as the tie-break.
    #[test]
    fn double_claims_agree_on_one_winner() {
        use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use kubimo::k8s_openapi::jiff::Timestamp;
        let pod = |name: &str, seconds: i64| Pod {
            metadata: kubimo::kube::api::ObjectMeta {
                name: Some(name.to_string()),
                creation_timestamp: Some(Time(Timestamp::new(seconds, 0).unwrap())),
                ..Default::default()
            },
            ..Default::default()
        };
        let older = pod("editors-bbbb", 100);
        let newer = pod("editors-aaaa", 200);
        let tied = pod("editors-cccc", 100);

        assert_eq!(claim_winner(&[]), None);
        assert_eq!(
            claim_winner(std::slice::from_ref(&newer)),
            Some("editors-aaaa")
        );
        // Age beats name…
        assert_eq!(
            claim_winner(&[newer.clone(), older.clone()]),
            Some("editors-bbbb")
        );
        // …and a timestamp tie falls back to the name, in either order.
        assert_eq!(
            claim_winner(&[tied.clone(), older.clone()]),
            claim_winner(&[older, tied]),
        );
    }

    /// Both reference styles the platform uses must be found, deduplicated.
    #[test]
    fn sidecar_secret_names_covers_env_and_env_from() {
        let container = Container {
            name: "api-proxy".into(),
            env: Some(vec![EnvVar {
                name: "API_KEY".into(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: "bmor-x-api-key".into(),
                        key: "key".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            env_from: Some(vec![EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: "bmor-x-api-key".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(sidecar_secret_names(&[container]), vec!["bmor-x-api-key"]);
    }
}
