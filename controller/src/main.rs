use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use futures::prelude::*;
use tower_http::BoxError;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt};

use kubimo_controller::{Config, Context, ControllerStreamExt, controllers};

/// Resolves when the process is asked to stop: SIGTERM as well as Ctrl+c
/// (SIGINT).
///
/// Kubernetes sends SIGTERM on a rollout. Reacting to Ctrl+c alone meant the
/// controller ran out its full terminationGracePeriodSeconds and was
/// SIGKILLed, during which an old and a new controller both reconciled and
/// fought over retired pod slot-volume attributes and warm-pool template
/// hashes (the chart's `strategy: Recreate` closes the other half of that
/// gap, by never running the new pod until the old one is gone).
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(err) => {
                tracing::error!(%err, "cannot listen for SIGTERM; falling back to SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn shutdown_signal(service: &'static str) {
    wait_for_shutdown_signal().await;
    tracing::info!("Shutting down {service} controller...");
}

async fn shutdown_timeout(timeout: Duration) -> Result<ExitCode, BoxError> {
    wait_for_shutdown_signal().await;
    tracing::info!("Shutting down gracefully... (Ctrl+c to force)");
    match tokio::time::timeout(timeout, wait_for_shutdown_signal()).await {
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

    #[cfg(feature = "metrics")]
    if config.metrics.enabled {
        #[cfg(feature = "metrics")]
        kubimo_controller::metrics::install(config.metrics.bind_addr);
    }

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
            controllers::workspace_directory::run(
                ctx.clone(),
                shutdown_signal("workspace_directory"),
            )
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
            controllers::budget::run(ctx.clone(), shutdown_signal("budget"))
                .await
                .unwrap()
                .wait(),
            controllers::pool::run(ctx.clone(), shutdown_signal("pool"))
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
