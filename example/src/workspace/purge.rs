use clap::Args;

use futures::prelude::*;
use kubimo::{KubimoWorkspace, prelude::*};

use crate::Context;

#[derive(Args)]
pub struct Purge {}

impl Purge {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        let bmows = context.client.api::<KubimoWorkspace>();
        bmows
            .list(&Default::default())
            .map_err(kubimo::Error::from)
            .try_for_each_concurrent(None, |item| {
                let bmows = context.client.api::<KubimoWorkspace>();
                async move {
                    bmows.delete(item.item.name()?).await?;
                    Ok(())
                }
            })
            .await?;
        Ok(())
    }
}
