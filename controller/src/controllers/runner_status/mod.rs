use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use futures::prelude::*;
use kubimo::k8s_openapi::{
    api::core::v1::Pod,
    apimachinery::pkg::apis::meta::v1::{Condition, Time},
    jiff::Timestamp,
};
use kubimo::kube::runtime::controller::Action;
use kubimo::{Runner, RunnerStatus, prelude::*};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::backoff::default_error_policy;
use crate::config::StatusCheckResolution;
use crate::context::Context;
use crate::controllers::ingress::ingress_path;
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
}

fn runner_api_endpoint(
    resolution: &StatusCheckResolution,
    runner: &Runner,
) -> Result<Url, RunnerStatusError> {
    Ok(match resolution {
        StatusCheckResolution::ServiceDns => Url::parse(&format!(
            "http://{name}.{namespace}.svc.cluster.local/{name}/",
            name = runner.name()?,
            namespace = runner.require_namespace()?
        ))?,
        StatusCheckResolution::Ingress { host } => {
            host.join(&format!("{}/", ingress_path(runner)?))?
        }
    }
    .join("api/")?)
}

pub struct RunnerApi {
    client: reqwest::Client,
    api_endpoint: Url,
}

impl RunnerApi {
    pub fn build(
        client: &reqwest::Client,
        runner: &Runner,
        resolution: &StatusCheckResolution,
    ) -> Result<Self, RunnerStatusError> {
        Ok(Self {
            client: client.clone(),
            api_endpoint: runner_api_endpoint(resolution, runner)?,
        })
    }

    pub async fn connections(&self) -> Result<Connections, RunnerStatusError> {
        Ok(self
            .client
            .get(self.api_endpoint.join("status/connections")?)
            .send()
            .await?
            .error_for_status()?
            .json::<Connections>()
            .await?)
    }

    pub async fn marimo_version(&self) -> Result<String, RunnerStatusError> {
        Ok(self
            .client
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
        let api = RunnerApi::build(&self.client, runner, &ctx.config.runner_status.resolution)?;
        let connections = match api.connections().await {
            Ok(connections) => connections,
            Err(err) => {
                tracing::warn!(err = ?err, "Could not get runner status: {}", err);
                return Ok(Some(Action::requeue(interval)));
            }
        };
        let marimo_version = if runner
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
        if connections.is_active() || marimo_version.is_some() {
            if connections.is_active() {
                status.last_active = Some(now);
            }
            if let Some(version) = marimo_version {
                status.marimo_version = Some(version)
            }
        }
        if !connections.is_active()
            && let Some(delete_after_secs_inactive) = runner
                .spec
                .lifecycle
                .as_ref()
                .and_then(|l| l.delete_after_secs_inactive)
        {
            let last_active_timestamp = runner
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
                .unwrap_or(Utc::now().timestamp());
            if last_active_timestamp + (delete_after_secs_inactive as i64) < now.timestamp() {
                ctx.api_for(runner)?.delete(runner.name()?).await?;
                return Ok(None);
            }
        }
        Ok(Some(Action::requeue(interval)))
    }

    async fn apply_pod_ready_condition(
        &self,
        ctx: &Context,
        runner: &Runner,
        status: &mut RunnerStatus,
    ) -> kubimo::Result<()> {
        const STATUS_TYPE: &str = "PodReady";
        let pod = ctx
            .api_namespaced::<Pod>(runner.require_namespace()?)
            .get_opt(runner.name()?)
            .await?;
        let (reason_str, message_str) = if let Some(pod) = pod {
            if let Some(condition) = pod
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .and_then(|conditions| conditions.iter().find(|c| c.type_ == "Ready"))
            {
                if condition.status == "True" {
                    ("Ready", "Ready")
                } else {
                    ("NotReady", "Not ready")
                }
            } else {
                ("NotStarted", "Not started")
            }
        } else {
            ("NotPresent", "Not present")
        };
        let status_str = if reason_str == "Ready" {
            "True"
        } else {
            "False"
        };
        if let Some(condition) = status
            .conditions
            .as_mut()
            .and_then(|c| c.iter_mut().find(|c| c.type_ == STATUS_TYPE))
        {
            if condition.reason != reason_str {
                condition.status = status_str.to_string();
                condition.reason = reason_str.to_string();
                condition.message = message_str.to_string();
                condition.observed_generation = runner.metadata.generation;
                condition.last_transition_time = Time(Timestamp::now());
            }
        } else {
            status
                .conditions
                .get_or_insert_with(Vec::new)
                .push(Condition {
                    type_: STATUS_TYPE.to_string(),
                    reason: reason_str.to_string(),
                    status: status_str.to_string(),
                    message: message_str.to_string(),
                    observed_generation: runner.metadata.generation,
                    last_transition_time: Time(Timestamp::now()),
                });
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Reconciler for RunnerStatusReconciler {
    type Resource = Runner;
    type Error = RunnerStatusError;

    async fn apply(&self, ctx: &Context, runner: &Runner) -> Result<Action, Self::Error> {
        let mut status = runner.status.clone().unwrap_or_default();
        let action = self.poll_api_status(ctx, runner, &mut status).await?;
        if let Some(action) = action {
            self.apply_pod_ready_condition(ctx, runner, &mut status)
                .await?;
            if Some(&status) != runner.status.as_ref() {
                let mut patched = runner.clone();
                patched.status = Some(status);
                ctx.api_for(runner)?.patch_status(&patched).await?;
            }
            Ok(action)
        } else {
            Ok(Action::await_change())
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
