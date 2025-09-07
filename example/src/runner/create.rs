use clap::{Args, ValueEnum};
use futures::{future::Either, prelude::*};
use kubimo::{
    FilterParams, KubimoRunner, KubimoRunnerCommand, KubimoRunnerSpec, KubimoWorkspace,
    WellKnownField,
    k8s_openapi::api::core::v1::{Pod, PodCondition},
    kube::runtime::watcher::Event,
    prelude::*,
};

use crate::Context;

#[derive(ValueEnum, Clone)]
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
    command: RunnerCommand,
    workspace: String,
    #[clap(long, default_value = "120")]
    startup_timeout_secs: u64,
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

impl Create {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        let bmows = context.client.api::<KubimoWorkspace>();
        let bmor = context.client.api::<KubimoRunner>();
        let workspace = bmows.get(&self.workspace).await?;
        let runner = bmor
            .patch(&workspace.create_runner(KubimoRunnerSpec {
                command: self.command.clone().into(),
                ..Default::default()
            })?)
            .await?;
        let name = runner.name()?;
        match futures::future::select(
            wait_for_pod(&context.client, name).boxed(),
            tokio::time::sleep(std::time::Duration::from_secs(self.startup_timeout_secs)).boxed(),
        )
        .await
        {
            Either::Left((res, _)) => res?,
            Either::Right((_, _)) => {
                return Err(format!("Timeout waiting for pod {name} to start").into());
            }
        };
        if let Some(ip) = context.minikube_ip {
            println!("http://{ip}/{name}");
        } else {
            println!("{name}");
        }
        Ok(())
    }
}
