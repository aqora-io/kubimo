use std::path::PathBuf;

use clap::Args;
use futures::prelude::*;
use git_url_parse::{GitUrl, Scheme};
use kubimo::{
    FilterParams, GitConfig, GitRepo, KubimoWorkspace, KubimoWorkspaceSpec, Requirement, S3Request,
    WellKnownField,
    k8s_openapi::api::batch::v1::{Job, JobCondition},
    kube::runtime::watcher::Event,
    prelude::*,
};
use url::Url;

use crate::Context;

#[derive(Args)]
pub struct Create {
    #[clap(short, long)]
    repo: Option<String>,
    #[clap(short, long)]
    branch: Option<String>,
    #[clap(short = 'v', long)]
    revision: Option<String>,
    #[clap(short, long)]
    ssh_key: Option<PathBuf>,
    #[clap(long, short)]
    module: Option<String>,
    #[clap(long, default_value = "10Gi")]
    min_storage: Option<String>,
    #[clap(long)]
    max_storage: Option<String>,
    #[clap(long)]
    git_name: Option<String>,
    #[clap(long)]
    git_email: Option<String>,
    #[clap(long)]
    s3: Option<String>,
    #[clap(long, default_value = "60")]
    job_timeout_secs: u64,
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

async fn wait_for_job(
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

impl Create {
    pub async fn run(mut self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        let spinner = crate::utils::spinner().with_message("Creating workspace");
        let timer = std::time::Instant::now();
        let bmows = context.client.api::<KubimoWorkspace>();
        if let Some(repo) = self.repo.as_deref() {
            let url = GitUrl::parse(repo)?;
            let user = if url.scheme == Scheme::File {
                let components = url.path.split('/').collect::<Vec<_>>();
                if components.len() != 2 {
                    return Err(format!("Invalid repo: {repo}").into());
                }
                self.repo = Some(format!(
                    "ssh://git@gitea-ssh.gitea.svc.cluster.local:2222/{}",
                    url.path
                ));
                Some(components[0].to_string())
            } else {
                url.user
            };
            if let Some(user) = user {
                if self.git_name.is_none() {
                    self.git_name = Some(user.to_string());
                }
                if self.git_email.is_none() {
                    self.git_email = Some(format!("{}@local.domain", user));
                }
            }
        }
        let workspace = bmows
            .patch(&KubimoWorkspace::create(KubimoWorkspaceSpec {
                storage: Some(Requirement {
                    min: self.min_storage.as_deref().map(|s| s.parse()).transpose()?,
                    max: self.max_storage.as_deref().map(|s| s.parse()).transpose()?,
                }),
                git_config: Some(GitConfig {
                    name: self.git_name,
                    email: self.git_email,
                }),
                repo: self
                    .repo
                    .map(|repo| {
                        if repo.contains(":") {
                            repo
                        } else {
                            format!("ssh://git@gitea-ssh.gitea.svc.cluster.local:2222/{repo}")
                        }
                    })
                    .map(|repo| GitRepo {
                        url: repo,
                        branch: self.branch,
                        revision: self.revision,
                    }),
                ssh_key: self
                    .ssh_key
                    .as_ref()
                    .map(std::fs::read_to_string)
                    .transpose()
                    .map_err(|e| format!("Failed to read ssh key: {}", e))?,
                s3_request: self
                    .s3
                    .as_ref()
                    .map(|s| Url::parse(s))
                    .transpose()?
                    .map(|url| S3Request {
                        url: Some(url),
                        ..Default::default()
                    }),
            }))
            .await?;
        let name = workspace.name()?;
        spinner.set_message(format!("Waiting for job {name}"));
        crate::utils::try_timeout(
            std::time::Duration::from_secs(self.job_timeout_secs),
            wait_for_job(&context.client, name),
        )
        .await?;
        spinner.finish_with_message(format!("Created in {:?}", timer.elapsed()));
        println!("{name}");
        Ok(())
    }
}
