use clap::Args;

use crate::{Context, Result};

#[derive(Args)]
pub struct Command {}

impl Command {
    pub async fn run(self, context: Context) -> Result<()> {
        Ok(())
    }
}
