use clap::Args;

use futures::prelude::*;
use kubimo::{Workspace, prelude::*};

use crate::Context;

#[derive(Args)]
pub struct Purge {}

impl Purge {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        let spinner = crate::utils::spinner().with_message("Purging workspaces");
        let timer = std::time::Instant::now();
        let bmows = context.client.api::<Workspace>();
        bmows
            .list(&Default::default())
            .map_err(kubimo::Error::from)
            .try_for_each_concurrent(None, |item| {
                let bmows = context.client.api::<Workspace>();
                let spinner = spinner.clone();
                async move {
                    let name = item.item.name()?;
                    spinner.set_message(format!("Deleting {name}"));
                    bmows.delete(name).await?;
                    Ok(())
                }
            })
            .await?;
        spinner.finish_with_message(format!("Deleted in {:?}", timer.elapsed()));
        Ok(())
    }
}
