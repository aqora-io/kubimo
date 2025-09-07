use clap::{Parser, Subcommand};

use crate::{Context, runner, workspace};

#[derive(Parser)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[clap(subcommand)]
    Workspace(workspace::Command),
    #[clap(subcommand)]
    Runner(runner::Command),
}

impl Command {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Workspace(workspace) => workspace.run(context).await,
            Self::Runner(runner) => runner.run(context).await,
        }
    }
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let context = Context::load().await?;
    args.command.run(&context).await?;
    Ok(())
}
