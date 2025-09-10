use clap::{Args, ValueEnum};
use futures::prelude::*;
use kubimo::{
    FilterParams, KubimoRunner, KubimoRunnerCommand, KubimoRunnerSpec, KubimoWorkspace,
    WellKnownField,
    k8s_openapi::api::core::v1::{Pod, PodCondition},
    kube::runtime::watcher::Event,
    prelude::*,
};
use url::Url;

use crate::Context;

#[derive(ValueEnum, Clone, Copy)]
pub enum RunnerCommand {
    Edit,
    Run,
}

impl From<RunnerCommand> for KubimoRunnerCommand {
    fn from(command: RunnerCommand) -> Self {
        match command {
            RunnerCommand::Edit => Self::Edit,
            RunnerCommand::Run => Self::Run,
        }
    }
}

#[derive(Args)]
pub struct Create {
    #[clap(long, default_value = "30")]
    startup_timeout_secs: u64,
    command: RunnerCommand,
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

async fn wait_for_endpoint(
    endpoint: &Url,
    polling_interval: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    loop {
        let instant = std::time::Instant::now();
        if let Ok(res) = client
            .head(endpoint.to_string())
            .timeout(polling_interval)
            .send()
            .await
            && res.status().as_u16() < 500
        {
            return Ok(());
        }
        tokio::time::sleep(polling_interval - instant.elapsed()).await;
    }
}

impl Create {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        let spinner = crate::utils::spinner().with_message("Creating runner");
        let timer = std::time::Instant::now();
        let bmows = context.client.api::<KubimoWorkspace>();
        let bmor = context.client.api::<KubimoRunner>();
        let workspace = bmows.get(&self.workspace).await?;
        let runner = bmor
            .patch(&workspace.create_runner(KubimoRunnerSpec {
                command: self.command.into(),
                ..Default::default()
            })?)
            .await?;
        let name = runner.name()?;
        spinner.set_message(format!("Waiting for pod {name}"));
        let mut duration = std::time::Duration::from_secs(self.startup_timeout_secs);
        let instant = std::time::Instant::now();
        crate::utils::try_timeout(duration, wait_for_pod(&context.client, name)).await?;
        duration -= instant.elapsed();
        let res = if let Some(ip) = context.minikube_ip {
            let mut url = Url::parse(&format!("http://{ip}/{name}/"))?;
            if let Some(notebook) = self.notebook {
                match self.command {
                    RunnerCommand::Edit => {
                        url.query_pairs_mut().append_pair("file", &notebook);
                    }
                    RunnerCommand::Run => {
                        let notebook = notebook
                            .rsplit_once('.')
                            .map(|(name, _)| name)
                            .unwrap_or(&notebook);
                        url = url.join(notebook)?;
                        url.query_pairs_mut().append_pair("show-code", "true");
                    }
                }
            }
            spinner.set_message(format!("Waiting for endpoint {url}"));
            crate::utils::try_timeout(
                duration,
                wait_for_endpoint(&url, std::time::Duration::from_millis(500)),
            )
            .await?;
            url.to_string()
        } else {
            name.to_string()
        };
        spinner.finish_with_message(format!("Created in {:?}", timer.elapsed()));
        println!("{res}");
        Ok(())
    }
}
