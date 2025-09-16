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
    tracing::info!("Received shutdown signal, shutting down {service} controller...");
}

async fn shutdown_timeout(timeout: Duration) -> Result<(), BoxError> {
    ctrl_c().await;
    tokio::time::sleep(timeout).await;
    tracing::warn!("Shutdown timeout reached, shutting down forcefully");
    Err(BoxError::from("Shutdown timeout reached"))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();
    let config = Config::load().unwrap();

    let mut builder = kubimo::Client::builder();
    builder.name(&config.name);
    let client = builder.build().await.unwrap();

    let ctx = Arc::new(Context::new(client.clone(), config));

    tracing::info!("Patching CRDs...");
    client.patch_all_crds().await.unwrap();

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
            controllers::exporter::run(ctx.clone(), shutdown_signal("exporter"))
                .await
                .unwrap()
                .wait(),
        ])
        .map(|_| Ok(())),
        shutdown_timeout(Duration::from_secs(60)).boxed(),
    )
    .await
    .map_err(|err| err.factor_first().0)
    .unwrap();
}
