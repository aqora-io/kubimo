use async_graphql::{extensions::Tracing, http::GraphiQLSource};
use async_graphql_axum::GraphQL;
use axum::{Router, response::Html, routing::get};
use tower_http::trace::TraceLayer;
use tracing_subscriber::prelude::*;

use kubimo_server::{Config, Schema, crd, kube_http};

lazy_static::lazy_static! {
    static ref GraphiQLHtml: Html<String> = Html(GraphiQLSource::build().endpoint("/").finish());
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
            get(|| async { GraphiQLHtml.clone() }).post_service(GraphQL::new(schema)),
        )
        .layer(TraceLayer::new_for_http());

    tracing::info!("Applying CRDs");
    crd::apply_all(&kube_client).await.unwrap();

    tracing::info!("Starting at {}:{}", server_config.host, server_config.port);
    let listener = tokio::net::TcpListener::bind((server_config.host, server_config.port))
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}
