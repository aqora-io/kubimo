mod create;
mod purge;

use clap::Subcommand;

use crate::Context;

#[derive(Subcommand)]
pub enum Command {
    Create(create::Create),
    Purge(purge::Purge),
}

impl Command {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Create(create) => create.run(context).await,
            Self::Purge(purge) => purge.run(context).await,
        }
    }
}
