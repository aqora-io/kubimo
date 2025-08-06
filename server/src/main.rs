use std::time::Duration;

use async_graphql::{extensions::Tracing, http::GraphiQLSource};
use async_graphql_axum::{GraphQL, GraphQLSubscription};
use axum::{Router, response::Html, routing::get};
use futures::prelude::*;
use tower_http::{BoxError, trace::TraceLayer};
use tracing_subscriber::prelude::*;

use kubimo_server::{Config, Schema, controller, crd, kube_http};

lazy_static::lazy_static! {
    static ref GraphiQLHtml: Html<String> = Html(GraphiQLSource::build().endpoint("/")
        .subscription_endpoint("/ws")
        .finish());
}

async fn wait_stream<'a>(stream: impl Stream + Send + 'a) {
    stream
        .for_each_concurrent(None, |_| futures::future::ready(()))
        .await
}

async fn shutdown_signal(service: &'static str) {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for shutdown signal");
    tracing::info!("Received shutdown signal, shutting down {service}...");
}

async fn shutdown_timeout(timeout: Duration) -> Result<(), BoxError> {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for shutdown signal");
    tokio::time::sleep(timeout).await;
    tracing::warn!("Shutdown timeout reached, shutting down...");
    Err(BoxError::from("Shutdown timeout reached, shutting down"))
}

#[tokio::main]
async fn main() {
    kubimo_server::tracing::subscriber().init();
    let server_config = Config::load().unwrap();
    let kube_config = kube::Config::infer().await.unwrap();
    let kube_service = kube_http::service_builder(&kube_config)
        .unwrap()
        .layer(TraceLayer::new_for_http())
        .service(kube_http::service(&kube_config).unwrap());
    let kube_client = kube::Client::new(
        kube_service,
        server_config
            .namespace
            .clone()
            .unwrap_or(kube_config.default_namespace),
    );

    let schema = Schema::build(Default::default(), Default::default(), Default::default())
        .extension(Tracing)
        .data(kube_client.clone())
        .data(server_config.clone())
        .finish();

    let app = Router::new()
        .route(
            "/",
            get(|| async { GraphiQLHtml.clone() }).post_service(GraphQL::new(schema.clone())),
        )
        .route_service("/ws", GraphQLSubscription::new(schema))
        .layer(TraceLayer::new_for_http());

    let controller_context =
        controller::context::ControllerContext::new(kube_client.clone(), server_config.clone());
    let workspace_controller = controller::workspace::run(
        controller_context.clone(),
        controller::workspace::controller(&controller_context)
            .graceful_shutdown_on(shutdown_signal("workspace_controller")),
    );

    tracing::info!("Starting at {}:{}", server_config.host, server_config.port);
    let listener = tokio::net::TcpListener::bind((server_config.host, server_config.port))
        .await
        .unwrap();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal("server"))
        .into_future();

    tracing::info!("Applying CRDs");
    crd::apply_all(&kube_client).await.unwrap();

    futures::future::try_select(
        shutdown_timeout(Duration::from_secs(60)).boxed(),
        futures::future::try_join_all([
            server.map_err(BoxError::from).boxed(),
            wait_stream(workspace_controller).map(|_| Ok(())).boxed(),
        ]),
    )
    .await
    .map_err(|err| err.factor_first().0)
    .unwrap();
}
