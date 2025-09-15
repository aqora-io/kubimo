use std::future::IntoFuture;
use std::time::Duration;

use futures::{TryFutureExt, prelude::*};
use indicatif::ProgressBar;
use kubimo::{
    FilterParams, WellKnownField,
    k8s_openapi::api::batch::v1::{Job, JobCondition},
    kube::runtime::watcher::Event,
};

pub fn spinner() -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner
}

pub async fn try_timeout<F>(
    duration: Duration,
    future: F,
) -> Result<<F::IntoFuture as TryFuture>::Ok, Box<dyn std::error::Error>>
where
    F: IntoFuture,
    F::IntoFuture: TryFuture,
    Box<dyn std::error::Error>: From<<F::IntoFuture as TryFuture>::Error>,
{
    Ok(
        match tokio::time::timeout(
            duration,
            TryFutureExt::into_future(IntoFuture::into_future(future)),
        )
        .await
        {
            Ok(res) => res?,
            Err(elapsed) => return Err(format!("Timeout: {elapsed}").into()),
        },
    )
}

fn job_completition_cond(job: &Job) -> Option<&JobCondition> {
    job.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())?
        .iter()
        .find(|c| c.type_ == "Complete")
}

fn assert_job_success(job: &Job) -> Result<(), String> {
    let cond = job_completition_cond(job).ok_or_else(|| "Job not completed".to_string())?;
    if cond.status == "True" {
        Ok(())
    } else {
        Err(cond
            .message
            .clone()
            .unwrap_or_else(|| "Unknown error".to_string()))
    }
}

pub async fn wait_for_job(
    client: &kubimo::Client,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let jobs = client.api::<Job>();
    let next = jobs
        .watch(&FilterParams::default().with_fields((WellKnownField::Name, name)))
        .try_filter_map(|event| {
            futures::future::ok(match event {
                Event::Apply(job) => Some(job),
                Event::InitApply(job) => Some(job),
                _ => None,
            })
        })
        .try_skip_while(|job| futures::future::ok(job_completition_cond(job).is_none()))
        .try_next()
        .await?
        .ok_or_else(|| format!("Job {name} not found"))?;
    assert_job_success(&next)?;
    Ok(())
}
