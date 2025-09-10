use futures::{future::Either, prelude::*};
use s3_tar::{Result, run};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();
    match futures::future::select(run().boxed(), tokio::signal::ctrl_c().boxed()).await {
        Either::Left((res, _)) => res?,
        Either::Right((res, _)) => {
            res?;
            println!("Ctrl-C received, exiting...")
        }
    }
    Ok(())
}
