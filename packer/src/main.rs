use std::process::ExitCode;

use clap::Parser;

use packer::{Command, Commands};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(err) = cli.command.run().await {
        println!("{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
