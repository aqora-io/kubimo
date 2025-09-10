use clap::{Parser, Subcommand};

use crate::context::{Context, GlobalArgs};
use crate::{Result, download, upload};

#[derive(Parser)]
struct Args {
    #[clap(subcommand)]
    command: Command,
    #[clap(flatten)]
    global: GlobalArgs,
}

#[derive(Subcommand)]
enum Command {
    Download(download::Command),
    Upload(upload::Command),
}

impl Command {
    pub async fn run(self, context: Context) -> Result<()> {
        match self {
            Self::Download(download) => download.run(context).await,
            Self::Upload(upload) => upload.run(context).await,
        }
    }
}

pub async fn run() -> Result<()> {
    let args = Args::parse();
    let context = Context::new(args.global);
    args.command.run(context).await?;
    Ok(())
}
