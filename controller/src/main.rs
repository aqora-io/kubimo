use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use tower_http::BoxError;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt};

use kubimo_controller::{Config, Context, ControllerStreamExt, controllers};

async fn ctrl_c() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for shutdown signal");
}

async fn shutdown_signal(service: &'static str) {
    ctrl_c().await;
    tracing::info!("Shutting down {service} controller...");
}

async fn shutdown_timeout(timeout: Duration) -> Result<ExitCode, BoxError> {
    ctrl_c().await;
    tracing::info!("Shutting down gracefully... (Ctrl+c to force)");
    match tokio::time::timeout(timeout, ctrl_c()).await {
        Ok(_) => {
            tracing::warn!("Ctrl+c signal received, shutting down forcefully");
            Ok(ExitCode::from(2))
        }
        Err(_) => {
            tracing::warn!("Shutdown timeout reached, shutting down forcefully");
            Err(BoxError::from("Shutdown timeout reached"))
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Could not install default crypto provider");
    let config = Config::load().unwrap();

    let mut builder = kubimo::Client::builder();
    builder.name(&config.manager_name);
    let client = builder.build().await.unwrap();

    let ctx = Arc::new(Context::new(client.clone(), config));

    tracing::info!(
        "Processing events in {} namespace...",
        client.kube().default_namespace()
    );
    futures::future::try_select(
        futures::future::join_all([
            controllers::workspace::run(ctx.clone(), shutdown_signal("workspace"))
                .await
                .unwrap()
                .wait(),
            controllers::runner::run(ctx.clone(), shutdown_signal("runner"))
                .await
                .unwrap()
                .wait(),
            controllers::runner_status::run(ctx.clone(), shutdown_signal("runner_status"))
                .await
                .unwrap()
                .wait(),
            controllers::cache_job::run(ctx.clone(), shutdown_signal("cache_job"))
                .await
                .unwrap()
                .wait(),
        ])
        .map(|_| Ok(ExitCode::SUCCESS)),
        shutdown_timeout(Duration::from_secs(60)).boxed(),
    )
    .await
    .map_err(|err| err.factor_first().0)
    .unwrap()
    .factor_first()
    .0
}
