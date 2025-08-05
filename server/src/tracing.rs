use tracing_subscriber::{
    EnvFilter, fmt,
    layer::{Layered, SubscriberExt},
    registry::Registry,
};

pub fn subscriber() -> Layered<EnvFilter, Layered<fmt::Layer<Registry>, Registry>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
}
