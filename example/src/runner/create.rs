use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use clap::{Args, ValueEnum};
use futures::prelude::*;
use kubimo::{
    FilterParams, Runner, RunnerCommand, RunnerSpec, WellKnownField, Workspace,
    k8s_openapi::api::core::v1::{Pod, PodCondition},
    kube::runtime::watcher::Event,
    prelude::*,
};
use serde::Deserialize;
use url::Url;

use crate::Context;

#[derive(ValueEnum, Clone, Copy)]
pub enum CommandArg {
    Edit,
    Run,
}

impl From<CommandArg> for RunnerCommand {
    fn from(command: CommandArg) -> Self {
        match command {
            CommandArg::Edit => Self::Edit,
            CommandArg::Run => Self::Run,
        }
    }
}

#[derive(Args)]
pub struct Create {
    #[clap(long, default_value = "30")]
    startup_timeout_secs: u64,
    command: CommandArg,
    workspace: String,
    notebook: Option<String>,
}

fn pod_ready_cond(pod: &Pod) -> Option<&PodCondition> {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())?
        .iter()
        .find(|c| c.type_ == "Ready")
}

async fn wait_for_pod(
    client: &kubimo::Client,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let jobs = client.api::<Pod>();
    let _ = jobs
        .watch(&FilterParams::default().with_fields((WellKnownField::Name, name)))
        .try_filter_map(|event| {
            futures::future::ok(match event {
                Event::Apply(job) => Some(job),
                Event::InitApply(job) => Some(job),
                _ => None,
            })
        })
        .try_skip_while(|pod| {
            futures::future::ok(pod_ready_cond(pod).is_none_or(|ready| ready.status != "True"))
        })
        .try_next()
        .await?
        .ok_or_else(|| format!("Pod {name} not found"))?;
    Ok(())
}

#[derive(Deserialize)]
struct Status {
    status: String,
}

async fn wait_for_endpoint(endpoint: &Url) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let fetch = || {
        let client = client.clone();
        async move {
            let status = client
                .get(endpoint.to_string())
                .send()
                .await?
                .error_for_status()?
                .json::<Status>()
                .await?;
            if status.status == "healthy" {
                Ok::<_, Box<dyn std::error::Error>>(())
            } else {
                Err(format!("Endpoint {endpoint} is not healthy").into())
            }
        }
    };
    fetch
        .retry(
            ExponentialBuilder::new()
                .with_min_delay(Duration::from_millis(100))
                .with_max_delay(Duration::from_secs(1))
                .without_max_times(),
        )
        .await?;
    Ok(())
}

impl Create {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        let spinner = crate::utils::spinner().with_message("Creating runner");
        let timer = std::time::Instant::now();
        let bmows = context.client.api::<Workspace>();
        let bmor = context.client.api::<Runner>();
        let workspace = bmows.get(&self.workspace).await?;
        let runner = bmor
            .patch(&workspace.create_runner(RunnerSpec {
                command: self.command.into(),
                ..Default::default()
            })?)
            .await?;
        let name = runner.name()?;
        spinner.set_message(format!("Waiting for pod {name}"));
        let mut duration = Duration::from_secs(self.startup_timeout_secs);
        let instant = std::time::Instant::now();
        crate::utils::try_timeout(duration, wait_for_pod(&context.client, name)).await?;
        duration -= instant.elapsed();
        let res = if let Some(ip) = context.minikube_ip {
            let mut url = Url::parse(&format!("http://{ip}/{name}/"))?;
            let health_url = match self.command {
                CommandArg::Edit => url.join("health")?,
                CommandArg::Run => url.join("_health")?,
            };
            spinner.set_message(format!("Waiting for endpoint {health_url}"));
            crate::utils::try_timeout(duration, wait_for_endpoint(&health_url)).await?;
            if let Some(notebook) = self.notebook {
                match self.command {
                    CommandArg::Edit => {
                        url.query_pairs_mut().append_pair("file", &notebook);
                    }
                    CommandArg::Run => {
                        let notebook = notebook
                            .rsplit_once('.')
                            .map(|(name, _)| name)
                            .unwrap_or(&notebook);
                        url = url.join(notebook)?;
                        url.query_pairs_mut().append_pair("show-code", "true");
                    }
                }
            }
            url.to_string()
        } else {
            name.to_string()
        };
        spinner.finish_with_message(format!("Created in {:?}", timer.elapsed()));
        println!("{res}");
        Ok(())
    }
}
