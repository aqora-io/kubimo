use hyper_rustls::HttpsConnector;
use hyper_util::{
    client::legacy::{Client as HttpClient, connect::HttpConnector},
    rt::TokioExecutor,
};
use kube::client::{
    ConfigExt as _,
    middleware::{AuthLayer, BaseUriLayer},
};
use tower::{
    ServiceBuilder,
    layer::util::{Identity, Stack},
    util::{Either, MapErrLayer},
};
use tower_http::BoxError;

pub fn service<B>(
    config: &kube::Config,
) -> kube::Result<HttpClient<HttpsConnector<HttpConnector>, B>>
where
    B: http_body::Body + Send,
    B::Data: Send,
{
    Ok(HttpClient::builder(TokioExecutor::new()).build(config.rustls_https_connector()?))
}

type OptionalAuthLayer = Either<AuthLayer, Identity>;
type ConfigLayers<I> = Stack<OptionalAuthLayer, Stack<BaseUriLayer, I>>;
type BoxErrorLayer<E> = MapErrLayer<fn(E) -> BoxError>;

pub fn service_builder<E>(
    config: &kube::Config,
) -> kube::Result<ServiceBuilder<Stack<BoxErrorLayer<E>, ConfigLayers<Identity>>>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    Ok(ServiceBuilder::new()
        .layer(config.base_uri_layer())
        .option_layer(config.auth_layer()?)
        .map_err(BoxError::from))
}
