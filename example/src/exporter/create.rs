use clap::Args;
use kubimo::{Exporter, ExporterSpec, S3Request, Workspace, prelude::*};
use url::Url;

use crate::Context;

#[derive(Args)]
pub struct Create {
    #[clap(long, default_value = "60")]
    job_timeout_secs: u64,
    workspace: String,
    s3: Url,
}

impl Create {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        let spinner = crate::utils::spinner().with_message("Creating runner");
        let timer = std::time::Instant::now();
        let bmows = context.client.api::<Workspace>();
        let bmoes = context.client.api::<Exporter>();
        let workspace = bmows.get(&self.workspace).await?;
        let runner = bmoes
            .patch(&workspace.create_exporter(ExporterSpec {
                s3_request: Some(S3Request {
                    url: Some(self.s3),
                    ..Default::default()
                }),
                ..Default::default()
            })?)
            .await?;
        let name = runner.name()?;
        spinner.set_message(format!("Waiting for job {name}"));
        crate::utils::try_timeout(
            std::time::Duration::from_secs(self.job_timeout_secs),
            crate::utils::wait_for_job(&context.client, name),
        )
        .await?;
        spinner.finish_with_message(format!("Created in {:?}", timer.elapsed()));
        println!("{name}");
        Ok(())
    }
}
