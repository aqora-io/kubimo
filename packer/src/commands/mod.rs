pub mod pack;
pub mod unpack;

use clap::Subcommand;
use thiserror::Error;

use pack::PackCommand;
use unpack::UnpackCommand;

#[async_trait::async_trait]
pub trait Command {
    type Error: std::error::Error;
    async fn run(self) -> Result<(), Self::Error>;
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Pack(PackCommand),
    Unpack(UnpackCommand),
}

#[derive(Debug, Error)]
pub enum CommandsError {
    #[error(transparent)]
    Pack(#[from] <PackCommand as Command>::Error),
    #[error(transparent)]
    Unpack(#[from] <UnpackCommand as Command>::Error),
}

#[async_trait::async_trait]
impl Command for Commands {
    type Error = CommandsError;
    async fn run(self) -> Result<(), Self::Error> {
        match self {
            Self::Pack(cmd) => cmd.run().await?,
            Self::Unpack(cmd) => cmd.run().await?,
        }
        Ok(())
    }
}
