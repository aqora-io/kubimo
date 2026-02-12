use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use futures::prelude::*;
use kubimo::kube::runtime::controller::Action;
use kubimo::{Runner, RunnerCommand, prelude::*};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::backoff::default_error_policy;
use crate::config::StatusCheckResolution;
use crate::context::Context;
use crate::error::ControllerResult;
use crate::reconciler::{ReconcileError, Reconciler, ReconcilerExt};

#[derive(Error, Debug)]
pub enum RunnerStatusError {
    #[error(transparent)]
    Kubimo(#[from] kubimo::Error),
    #[error("Bad url: {0}")]
    Url(#[from] url::ParseError),
    #[error("Could not request status: {0}")]
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
        StatusCheckResolution::Ingress { host } => host.join(&format!("{}/", runner.name()?))?,
    }
    .join(match runner.spec.command {
        RunnerCommand::Edit => "api/",
        RunnerCommand::Run => "_api/",
    })?)
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

async fn runner_status(
    client: &reqwest::Client,
    api_endpoint: &Url,
) -> Result<Connections, RunnerStatusError> {
    Ok(client
        .get(api_endpoint.join("status/connections")?)
        .send()
        .await?
        .json::<Connections>()
        .await?)
}

#[derive(Debug, Clone, Default)]
struct RunnerStatusReconciler {
    client: reqwest::Client,
}

#[async_trait::async_trait]
impl Reconciler for RunnerStatusReconciler {
    type Resource = Runner;
    type Error = RunnerStatusError;

    async fn apply(&self, ctx: &Context, runner: &Runner) -> Result<Action, Self::Error> {
        let interval = Duration::from_secs(ctx.config.runner_status.interval_secs);
        let now = Utc::now();
        if let Some(last_active) = runner.status.as_ref().and_then(|s| s.last_active)
            && (now - last_active) < TimeDelta::from_std(interval).unwrap_or(TimeDelta::MAX)
        {
            return Ok(Action::requeue(interval));
        }
        let connections = match runner_status(
            &self.client,
            &runner_api_endpoint(&ctx.config.runner_status.resolution, runner)?,
        )
        .await
        {
            Ok(connections) => connections,
            Err(err) => {
                tracing::warn!("Could not get runner status: {:?}", err);
                return Ok(Action::requeue(interval));
            }
        };
        let bmors = ctx.api_for(runner)?;
        if connections.is_active() {
            let mut patched = runner.clone();
            patched.status.get_or_insert_default().last_active = Some(now);
            bmors.patch_status(&patched).await?;
        } else if let Some(delete_after_secs_inactive) = runner
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
                bmors.delete(runner.name()?).await?;
            }
        }
        Ok(Action::requeue(interval))
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
