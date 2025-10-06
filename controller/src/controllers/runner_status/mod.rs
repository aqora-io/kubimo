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
            "http://{}.{}.svc.cluster.local",
            runner.name()?,
            runner.require_namespace()?
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
        if connections.is_active() {
            let mut patched = runner.clone();
            patched.status.get_or_insert_default().last_active = Some(now);
            ctx.api_namespaced::<Runner>(patched.require_namespace()?)
                .patch_status(&patched)
                .await?;
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
