use clap::{Parser, Subcommand};

use crate::{
    context::{Context, GlobalArgs},
    exporter, runner, workspace,
};

#[derive(Parser)]
struct Args {
    #[clap(subcommand)]
    command: Command,
    #[clap(flatten)]
    global: GlobalArgs,
}

#[derive(Subcommand)]
enum Command {
    #[clap(subcommand)]
    Workspace(workspace::Command),
    #[clap(subcommand)]
    Runner(runner::Command),
    #[clap(subcommand)]
    Exporter(exporter::Command),
}

impl Command {
    pub async fn run(self, context: &Context) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Workspace(workspace) => workspace.run(context).await,
            Self::Runner(runner) => runner.run(context).await,
            Self::Exporter(exporter) => exporter.run(context).await,
        }
    }
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let context = Context::load(args.global).await?;
    args.command.run(&context).await?;
    Ok(())
}
