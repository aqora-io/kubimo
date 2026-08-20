//! Keeps each Pool's fleet of warm pods at its desired size.
//!
//! Warm pods are created here and *leave* here: a claim (runner reconciler)
//! flips a pod's state label and swaps its controller ownerReference to the
//! Runner, after which this loop only counts it. Both sides move pods between
//! states with a JSON-Patch `test` on the previous state label, so a claim and
//! a retire can race and exactly one wins.

pub(crate) mod warm_pod;

use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use kubimo::k8s_openapi::api::core::v1::{Pod, Secret};
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kubimo::k8s_openapi::jiff::Timestamp;
use kubimo::kube::runtime::{Controller, controller::Action, reflector::ObjectRef, watcher};
use kubimo::pool::{
    POOL_LABEL, POOL_STATE_CLAIMED, POOL_STATE_LABEL, POOL_STATE_RETIRING, POOL_STATE_WARM,
    POOL_TEMPLATE_HASH_ANNOTATION,
};
use kubimo::{Api, FilterParams, Pool, PoolStatus, json_patch_macros::*, prelude::*};

use crate::backoff::default_error_policy;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

/// Counts refresh on a timer as well as on pod events: a claim only patches
/// the pod, so nothing else guarantees this pool a wakeup (cf. `budget`).
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

const READY: &str = "Ready";

#[derive(Debug, Clone, Copy)]
struct PoolReconciler;

#[async_trait::async_trait]
impl Reconciler for PoolReconciler {
    type Resource = Pool;
    type Error = kubimo::Error;

    async fn apply(&self, ctx: &Context, pool: &Pool) -> Result<Action, Self::Error> {
        let namespace = pool.require_namespace()?;
        let name = pool.name()?;
        let pods = ctx.api_namespaced::<Pod>(namespace);
        let secrets = ctx.api_namespaced::<Secret>(namespace);

        let fleet: Vec<Pod> = pods
            .list(&FilterParams::new().with_labels((POOL_LABEL, name)))
            .map_ok(|item| item.item)
            .try_collect()
            .await?;

        let template_hash = warm_pod::template_hash(&ctx.config, pool);
        let mut warm = Vec::new();
        let mut retiring = Vec::new();
        let mut claimed = 0u32;
        for pod in fleet {
            if pod.metadata.deletion_timestamp.is_some() {
                continue;
            }
            match pod
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(POOL_STATE_LABEL))
                .map(String::as_str)
            {
                Some(POOL_STATE_WARM) => warm.push(pod),
                Some(POOL_STATE_CLAIMED) => claimed += 1,
                Some(POOL_STATE_RETIRING) => retiring.push(pod),
                // A pool-labelled pod without a state is not ours to manage —
                // most likely a newer controller's; leave it alone.
                _ => {}
            }
        }

        // Oldest first: the oldest warm pods are the most likely to be fully
        // booted, so they are the ones to keep (and the ones claims prefer).
        warm.sort_by_key(|pod| pod.metadata.creation_timestamp.clone());
        let (kept, excess): (Vec<&Pod>, Vec<&Pod>) = {
            let (fresh, drifted): (Vec<&Pod>, Vec<&Pod>) = warm.iter().partition(|pod| {
                pod.metadata
                    .annotations
                    .as_ref()
                    .and_then(|annotations| annotations.get(POOL_TEMPLATE_HASH_ANNOTATION))
                    == Some(&template_hash)
            });
            let mut excess = drifted;
            let keep = (pool.spec.replicas as usize).min(fresh.len());
            excess.extend_from_slice(&fresh[keep..]);
            (fresh[..keep].to_vec(), excess)
        };

        for pod in &excess {
            retire(&pods, pod.name()?).await?;
        }
        // Pods a previous reconcile retired but did not get to delete.
        for pod in &retiring {
            pods.delete_opt(pod.name()?).await?;
        }

        // A crash between pod- and Secret-create leaves a warm pod whose
        // sidecars wait on a mount that will never appear; recreate it.
        // A bare create, not get-then-apply: a claim may copy sidecar data
        // into the Secret between the two steps, and re-applying the empty
        // template under the same field manager would relinquish — i.e.
        // delete — that data. POST is atomic: an existing Secret, whatever it
        // holds by now, answers 409 and is left untouched.
        for pod in &kept {
            match secrets
                .kube()
                .create(&Default::default(), &warm_pod::claim_secret(pod)?)
                .await
            {
                Ok(_) => {}
                Err(kubimo::kube::Error::Api(status)) if status.code == 409 => {}
                Err(err) => return Err(err.into()),
            }
        }

        let deficit = (pool.spec.replicas as usize).saturating_sub(kept.len());
        for _ in 0..deficit {
            let identity = warm_pod::mint_identity(name);
            // Create, never converge: a warm pod's command embeds its minted
            // token, so re-applying an existing pod would always try to
            // mutate an immutable field. A name collision 409s and the next
            // reconcile re-mints.
            let created = pods
                .patch(&warm_pod::build_warm_pod(&ctx.config, pool, &identity)?)
                .await?;
            secrets.patch(&warm_pod::claim_secret(&created)?).await?;
        }

        let mut patched = pool.clone();
        patched.status = Some(PoolStatus {
            // Ready means the fleet was already at size before this pass —
            // pods minted just now report on the next one, once they exist.
            conditions: Some(vec![ready_condition(pool, deficit == 0)]),
            warm: Some(kept.len() as u32),
            claimed: Some(claimed),
        });
        ctx.api_namespaced::<Pool>(namespace)
            .patch_status(&patched)
            .await?;

        Ok(Action::requeue(REFRESH_INTERVAL))
    }

    // Cleanup is the default no-op on purpose. Warm and retiring pods carry a
    // controller ownerReference to the Pool, so garbage collection removes
    // them (and their claim Secrets, owned by the pods). Claimed pods are
    // deliberately left alone: their ownerReference was swapped to the Runner
    // at claim time, so deleting a Pool never takes down a notebook someone is
    // sitting in.
}

/// Withdraw a warm pod from the claimable set, then delete it.
///
/// The `test` on the state label is what makes this safe against a concurrent
/// claim: whichever patch lands second fails with a 422 and the pod is
/// unambiguously claimed or retiring, never both. Losing the race is not an
/// error — the pod is simply no longer ours.
async fn retire(pods: &Api<Pod>, name: &str) -> Result<(), kubimo::Error> {
    let withdrawn = pods
        .patch_json(
            name,
            patch![
                test!(["metadata", "labels", POOL_STATE_LABEL] => POOL_STATE_WARM),
                put!(["metadata", "labels", POOL_STATE_LABEL] => POOL_STATE_RETIRING),
            ],
        )
        .await;
    match withdrawn {
        Ok(_) => {
            pods.delete_opt(name).await?;
            Ok(())
        }
        Err(err) if crate::controllers::runner::is_invalid_request(&err) => {
            tracing::info!(pod = name, "warm pod was claimed before it could retire");
            Ok(())
        }
        // Already gone: nothing to retire.
        Err(kubimo::Error::Kube(kubimo::kube::Error::Api(status))) if status.code == 404 => Ok(()),
        Err(err) => Err(err),
    }
}

/// `Ready` condition, preserving the previous transition time when the status
/// is unchanged (cf. `budget::exceeded_condition`).
fn ready_condition(pool: &Pool, ready: bool) -> Condition {
    let (status, reason, message) = if ready {
        ("True", "Ready", "All warm pods are minted")
    } else {
        ("False", "Filling", "Minting warm pods up to replicas")
    };
    let previous = pool
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .and_then(|conditions| conditions.iter().find(|cond| cond.type_ == READY));
    let last_transition_time = match previous {
        Some(previous) if previous.status == status => previous.last_transition_time.clone(),
        _ => Time(Timestamp::now()),
    };
    Condition {
        last_transition_time,
        observed_generation: pool.metadata.generation,
        message: message.into(),
        reason: reason.into(),
        status: status.into(),
        type_: READY.into(),
    }
}

pub fn controller(ctx: &Context) -> Controller<Pool> {
    let pools = ctx.api_global::<Pool>().kube().clone();
    let pods = ctx.api_global::<Pod>().kube().clone();
    Controller::new(pools, Default::default())
        // `watches` with a label mapper rather than `owns`: the claim swaps a
        // pod's controller ownerReference from the Pool to the Runner, so
        // ownership-based mapping would go blind at exactly the event this
        // pool most needs — "one of my warm pods was just taken".
        // Existence selector: only pool-labelled pods reach the mapper, so
        // every other pod churn in the cluster stays off this watch.
        .watches(pods, watcher::Config::default().labels(POOL_LABEL), |pod| {
            let namespace = pod.metadata.namespace.clone()?;
            let pool = pod.metadata.labels.as_ref()?.get(POOL_LABEL)?;
            Some(ObjectRef::new(pool).within(&namespace))
        })
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<Pool, ReconcileError<kubimo::Error>>>,
    ReconcileError<kubimo::Error>,
> {
    Ok(controller(&ctx).graceful_shutdown_on(shutdown_signal).run(
        PoolReconciler.reconcile("controller").await?,
        default_error_policy,
        ctx,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retire patch must lead with the `test` op — that is the whole
    /// mutual-exclusion mechanism against a concurrent claim.
    #[test]
    fn retire_patch_is_guarded_by_a_state_test() {
        let patch = patch![
            test!(["metadata", "labels", POOL_STATE_LABEL] => POOL_STATE_WARM),
            put!(["metadata", "labels", POOL_STATE_LABEL] => POOL_STATE_RETIRING),
        ];
        let json = serde_json::to_value(&patch).unwrap();
        assert_eq!(json[0]["op"], "test");
        assert_eq!(
            json[0]["path"],
            "/metadata/labels/kubimo.aqora.io~1pool-state"
        );
        assert_eq!(json[0]["value"], POOL_STATE_WARM);
        assert_eq!(json[1]["op"], "replace");
        assert_eq!(json[1]["value"], POOL_STATE_RETIRING);
    }
}
