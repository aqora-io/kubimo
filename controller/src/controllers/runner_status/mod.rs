mod apply_conditions;
mod conditions;

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use futures::prelude::*;
use kubimo::k8s_openapi::api::core::v1::Secret;
use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition as K8sCondition;
use kubimo::kube::runtime::controller::Action;
use kubimo::{Runner, RunnerCommand, RunnerStatus, prelude::*};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::backoff::default_error_policy;
use crate::config::StatusCheckResolution;
use crate::context::Context;
use crate::controllers::ingress::effective_ingress_path;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Error, Debug)]
pub enum RunnerStatusError {
    #[error(transparent)]
    Kubimo(#[from] kubimo::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error("token secret {secret} is missing key {key}")]
    TokenSecretKeyMissing { secret: String, key: String },
    #[error("token secret {secret} key {key} is not valid utf-8")]
    TokenSecretKeyInvalidUtf8 { secret: String, key: String },
}

fn runner_api_endpoint(
    resolution: &StatusCheckResolution,
    runner: &Runner,
) -> Result<Url, RunnerStatusError> {
    Ok(match resolution {
        StatusCheckResolution::ServiceDns => Url::parse(&format!(
            "http://{name}.{namespace}.svc.cluster.local/",
            name = runner.name()?,
            namespace = runner.require_namespace()?,
        ))?,
        StatusCheckResolution::Ingress { host } => host.clone(),
    }
    .join(&format!("{}/", effective_ingress_path(runner)?))?
    .join("api/")?)
}

async fn resolve_token(
    ctx: &Context,
    runner: &Runner,
) -> Result<Option<String>, RunnerStatusError> {
    // A claimed warm pod authenticates with the token minted at its birth;
    // whatever the spec asked for was never given to marimo.
    if let Some(claim) = runner.status.as_ref().and_then(|s| s.claim.as_ref()) {
        return Ok(claim.token.clone());
    }
    let Some(token_spec) = runner.spec.token.as_ref() else {
        return Ok(None);
    };
    if let Some(value) = token_spec.value.as_ref() {
        return Ok(Some(value.clone()));
    }
    let Some(secret_ref) = token_spec.secret_ref.as_ref() else {
        return Ok(None);
    };
    let namespace = runner.require_namespace()?;
    let secret = ctx
        .api_namespaced::<Secret>(namespace)
        .get(&secret_ref.name)
        .await?;
    let bytes = secret
        .data
        .as_ref()
        .and_then(|data| data.get(&secret_ref.key))
        .ok_or_else(|| RunnerStatusError::TokenSecretKeyMissing {
            secret: secret_ref.name.clone(),
            key: secret_ref.key.clone(),
        })?;
    let value = String::from_utf8(bytes.0.clone()).map_err(|_| {
        RunnerStatusError::TokenSecretKeyInvalidUtf8 {
            secret: secret_ref.name.clone(),
            key: secret_ref.key.clone(),
        }
    })?;
    Ok(Some(value))
}

pub struct RunnerApi {
    client: reqwest::Client,
    api_endpoint: Url,
    token: Option<String>,
}

impl RunnerApi {
    pub async fn build(
        ctx: &Context,
        client: &reqwest::Client,
        runner: &Runner,
        resolution: &StatusCheckResolution,
    ) -> Result<Self, RunnerStatusError> {
        Ok(Self {
            client: client.clone(),
            api_endpoint: runner_api_endpoint(resolution, runner)?,
            token: resolve_token(ctx, runner).await?,
        })
    }

    fn get(&self, url: Url) -> reqwest::RequestBuilder {
        let req = self.client.get(url);
        match &self.token {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    pub async fn connections(&self) -> Result<Connections, RunnerStatusError> {
        Ok(self
            .get(self.api_endpoint.join("status/connections")?)
            .send()
            .await?
            .error_for_status()?
            .json::<Connections>()
            .await?)
    }

    pub async fn marimo_version(&self) -> Result<String, RunnerStatusError> {
        Ok(self
            .get(self.api_endpoint.join("version")?)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    }
}

#[derive(Debug, Deserialize)]
pub struct Connections {
    active: usize,
}

impl Connections {
    #[inline]
    pub fn is_active(&self) -> bool {
        self.active > 0
    }
}

/// Requeue interval while startup conditions are not all True. PVC and
/// Workspace changes don't trigger this controller's watches, so a faster
/// requeue is what surfaces startup progress promptly.
pub(super) const STARTUP_REQUEUE_INTERVAL: Duration = Duration::from_secs(3);

/// How long a pod must have been un-ready before an unreachable runner may be
/// collected. Cheap insurance against a node blip flipping `PodReady` for a few
/// seconds in the middle of an ingress outage.
const UNREACHABLE_GC_GRACE_SECS: i64 = 5 * 60;

/// May a runner we could not reach be treated as idle?
///
/// Only when kubelet agrees it is unhealthy, and has for a while. A pod that is
/// `Ready` while the HTTP poll fails is an infrastructure problem — the runner
/// is fine and someone may well be using it.
fn unreachable_is_collectable(conditions: &[K8sCondition], now_secs: i64) -> bool {
    !conditions::pod_is_ready(conditions)
        && conditions::pod_ready_age_secs(conditions, now_secs)
            .is_some_and(|age| age >= UNREACHABLE_GC_GRACE_SECS)
}

/// Has this runner been idle long enough to collect?
///
/// Falls back to the creation timestamp: a runner that never came up has no
/// `lastActive` and would otherwise never expire, which is how runners reached
/// 8,000 restarts over 53 days without being cleaned up.
fn is_inactive_past_deadline(
    runner: &Runner,
    delete_after_secs_inactive: u32,
    now_secs: i64,
) -> bool {
    let Some(since) = runner
        .status
        .as_ref()
        .and_then(|status| status.last_active.map(|dt| dt.timestamp()))
        .or_else(|| {
            runner
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.as_second())
        })
    else {
        // Neither timestamp: nothing to measure against, so never collect.
        return false;
    };
    since + (delete_after_secs_inactive as i64) < now_secs
}

#[derive(Debug, Clone, Default)]
struct RunnerStatusReconciler {
    client: reqwest::Client,
}

impl RunnerStatusReconciler {
    async fn poll_api_status(
        &self,
        ctx: &Context,
        runner: &Runner,
        status: &mut RunnerStatus,
    ) -> Result<Option<Action>, RunnerStatusError> {
        let interval = Duration::from_secs(ctx.config.runner_status.interval_secs);
        let now = Utc::now();
        if let Some(last_active) = runner.status.as_ref().and_then(|s| s.last_active)
            && (now - last_active) < TimeDelta::from_std(interval).unwrap_or(TimeDelta::MAX)
        {
            return Ok(Some(Action::requeue(interval)));
        }
        let api = RunnerApi::build(
            ctx,
            &self.client,
            runner,
            &ctx.config.runner_status.resolution,
        )
        .await?;
        // A failed poll is ambiguous: "this runner is dead" and "the controller
        // cannot reach it" look identical from here. Carry on rather than
        // returning, so the idle GC below stays reachable — the pod's own
        // readiness is what tells the two apart.
        let connections = match api.connections().await {
            Ok(connections) => Some(connections),
            Err(err) => {
                // Unreachable-and-not-ready is the ordinary case — the pod is
                // starting, or crashlooping — and says nothing new every few
                // seconds. A *ready* pod we cannot reach is the interesting
                // one: that is an ingress or DNS fault, not a runner fault.
                if conditions::pod_is_ready(status.conditions.as_deref().unwrap_or_default()) {
                    tracing::warn!(err = ?err, "Could not reach a ready runner: {}", err);
                } else {
                    tracing::debug!(err = ?err, "Could not reach a runner whose pod is not ready: {}", err);
                }
                None
            }
        };
        let is_active = connections.as_ref().is_some_and(Connections::is_active);
        let marimo_version = if connections.is_some()
            && runner
                .status
                .as_ref()
                .is_none_or(|status| status.marimo_version.is_none())
        {
            match api.marimo_version().await {
                Ok(version) => Some(version),
                Err(err) => {
                    tracing::warn!(err = ?err, "Could not get runner version: {}", err);
                    None
                }
            }
        } else {
            None
        };
        if is_active {
            status.last_active = Some(now);
        }
        if let Some(version) = marimo_version {
            status.marimo_version = Some(version)
        }
        if !is_active
            && let Some(delete_after_secs_inactive) = runner
                .spec
                .lifecycle
                .as_ref()
                .and_then(|l| l.delete_after_secs_inactive)
            // An unreachable runner may only be treated as idle when its pod is
            // also not ready, and has been for long enough that a momentary
            // flip collects nothing. With `Ingress` resolution a single outage
            // makes every runner unreachable at once; without this, an outage
            // outlasting `deleteAfterSecsInactive` would delete every runner in
            // the cluster, including ones with users connected.
            && (connections.is_some()
                || unreachable_is_collectable(
                    status.conditions.as_deref().unwrap_or_default(),
                    now.timestamp(),
                ))
            && is_inactive_past_deadline(runner, delete_after_secs_inactive, now.timestamp())
        {
            let name = runner.name()?;
            // Nothing else records this at info level — `Api::delete` traces at
            // debug — so a user's runner otherwise vanishes without a word, and
            // "where did my notebook go" has no answer. Log the inputs the
            // decision was actually made on, including whether the poll reached
            // the runner at all: an unreachable-but-collectable runner is a very
            // different story from an idle one.
            tracing::info!(
                runner = %name,
                is_active,
                delete_after_secs_inactive,
                reachable = connections.is_some(),
                pod_ready_age_secs = ?conditions::pod_ready_age_secs(
                    status.conditions.as_deref().unwrap_or_default(),
                    now.timestamp(),
                ),
                "Deleting an inactive runner",
            );
            ctx.api_for(runner)?.delete(name).await?;
            return Ok(None);
        }
        Ok(Some(Action::requeue(interval)))
    }
}

#[async_trait::async_trait]
impl Reconciler for RunnerStatusReconciler {
    type Resource = Runner;
    type Error = RunnerStatusError;

    async fn apply(&self, ctx: &Context, runner: &Runner) -> Result<Action, Self::Error> {
        let mut status = runner.status.clone().unwrap_or_default();
        let startup_complete = self
            .apply_startup_conditions(ctx, runner, &mut status)
            .await?;
        // A claimed runner has no Service until the agent acks the claim, so
        // polling would only fail against a pod that is otherwise Ready and
        // warn-log every few seconds about an outage that isn't one.
        let claim_pending = runner
            .status
            .as_ref()
            .and_then(|s| s.claim.as_ref())
            .is_some()
            && !conditions::volume_is_bound(status.conditions.as_deref().unwrap_or_default());
        let action = if matches!(runner.spec.command, RunnerCommand::Render) || claim_pending {
            Action::await_change()
        } else {
            let action = self.poll_api_status(ctx, runner, &mut status).await?;
            let Some(action) = action else {
                // Runner was deleted for inactivity
                return Ok(Action::await_change());
            };
            action
        };
        // Computed before `status` is moved into the patch below.
        let startup_requeue = conditions::startup_requeue_interval(
            status.conditions.as_deref().unwrap_or_default(),
            Utc::now().timestamp(),
            Duration::from_secs(ctx.config.runner_status.interval_secs),
        );
        if Some(&status) != runner.status.as_ref() {
            let mut patched = runner.clone();
            patched.status = Some(status);
            ctx.api_for(runner)?.patch_status(&patched).await?;
        }
        if startup_complete {
            Ok(action)
        } else {
            Ok(Action::requeue(startup_requeue))
        }
    }
}

pub async fn run(
    ctx: Arc<Context>,
    shutdown_signal: impl Future<Output = ()> + Send + Sync + 'static,
) -> Result<
    impl Stream<Item = ControllerResult<Runner, ReconcileError<RunnerStatusError>>>,
    ReconcileError<RunnerStatusError>,
> {
    Ok(crate::controllers::runner::controller(&ctx)
        .graceful_shutdown_on(shutdown_signal)
        .run(
            RunnerStatusReconciler::default()
                .reconcile("runner_status")
                .await?,
            default_error_policy,
            ctx,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use kubimo::k8s_openapi::jiff::Timestamp;

    fn pod_ready_at(status: &str, transitioned_secs: i64) -> Vec<K8sCondition> {
        vec![K8sCondition {
            type_: kubimo::conditions::POD_READY.to_string(),
            status: status.to_string(),
            reason: "Test".to_string(),
            message: String::new(),
            last_transition_time: Time(Timestamp::from_second(transitioned_secs).unwrap()),
            observed_generation: None,
        }]
    }

    fn runner_created_at(secs: i64, last_active: Option<i64>) -> Runner {
        let mut runner = Runner::new("bmor-test", Default::default());
        runner.metadata.creation_timestamp = Some(Time(Timestamp::from_second(secs).unwrap()));
        runner.status = last_active.map(|secs| RunnerStatus {
            last_active: chrono::DateTime::from_timestamp(secs, 0),
            ..Default::default()
        });
        runner
    }

    /// The incident this guard exists to prevent. With `Ingress` resolution one
    /// outage makes `connections()` fail for *every* runner at once; if the
    /// outage outlasts `deleteAfterSecsInactive`, an unguarded GC would delete
    /// the entire cluster's runners, including ones with users connected.
    /// Kubelet still reports those pods `Ready`, which is what saves them.
    #[test]
    fn an_unreachable_runner_with_a_ready_pod_is_never_collected() {
        let conditions = pod_ready_at("True", 0);
        assert!(!unreachable_is_collectable(&conditions, 100 * 3600));
    }

    /// A pod that only just went un-ready might be mid-restart or mid-blip.
    /// Wait out the grace before believing it.
    #[test]
    fn an_unreachable_runner_with_a_freshly_unready_pod_is_not_collected_yet() {
        let start = 1_000;
        assert!(!unreachable_is_collectable(
            &pod_ready_at("False", start),
            start + 60
        ));
        // And a runner with no PodReady condition at all is not collectable:
        // there is no evidence either way.
        assert!(!unreachable_is_collectable(&[], start + 100 * 3600));
    }

    /// The 12 production runners: `PodReady=False` for weeks.
    #[test]
    fn an_unreachable_runner_unready_for_hours_is_collected() {
        let start = 1_000;
        assert!(unreachable_is_collectable(
            &pod_ready_at("False", start),
            start + 24 * 3600
        ));
    }

    /// A runner that never came up has no `lastActive`, so without the
    /// creation-timestamp fallback its deadline never arrives — which is how
    /// runners reached 8,000 restarts over 53 days uncollected.
    #[test]
    fn a_runner_that_was_never_active_expires_from_its_creation() {
        let day = 86_400;
        let created = 1_000;
        let runner = runner_created_at(created, None);
        assert!(!is_inactive_past_deadline(
            &runner,
            day as u32,
            created + 60
        ));
        assert!(is_inactive_past_deadline(
            &runner,
            day as u32,
            created + day + 1
        ));
    }

    /// A recent `lastActive` must win over an old creation timestamp, or a
    /// long-lived runner in active use would be collected.
    #[test]
    fn last_active_takes_precedence_over_creation() {
        let day = 86_400;
        let created = 1_000;
        let now = created + 30 * day;
        let runner = runner_created_at(created, Some(now - 60));
        assert!(!is_inactive_past_deadline(&runner, day as u32, now));
    }
}
