mod create;

use clap::Subcommand;

use crate::Context;

#[derive(Subcommand)]
pub enum Command {
    Create(Box<create::Create>),
}

impl Command {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Create(create) => create.run(context).await,
        }
    }
}
