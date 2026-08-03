use std::net::SocketAddr;
use std::time::Instant;

use futures::future::{BoxFuture, FutureExt};
use tower::{Layer, Service};

pub fn install(bind_addr: SocketAddr) {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(bind_addr)
        .install()
        .expect("failed to install Prometheus metrics exporter");
    tracing::info!("Serving Prometheus metrics on http://{bind_addr}/metrics");
}

pub(crate) fn controller_name<T>() -> &'static str {
    let full = std::any::type_name::<T>();
    full.rsplit("::").next().unwrap_or(full)
}

#[derive(Debug, Clone, Copy)]
pub struct MetricsLayer {
    controller: &'static str,
}

impl MetricsLayer {
    pub fn new(controller: &'static str) -> Self {
        Self { controller }
    }
}

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        MetricsService {
            inner,
            controller: self.controller,
        }
    }
}

pub struct MetricsService<S> {
    inner: S,
    controller: &'static str,
}

impl<S, T, C> Service<(std::sync::Arc<T>, std::sync::Arc<C>)> for MetricsService<S>
where
    S: Service<(std::sync::Arc<T>, std::sync::Arc<C>)> + Send,
    T: Send + Sync,
    C: Send + Sync,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: (std::sync::Arc<T>, std::sync::Arc<C>)) -> Self::Future {
        let controller = self.controller;
        let start = Instant::now();
        let fut = self.inner.call(req);
        async move {
            let result = fut.await;
            let elapsed = start.elapsed().as_secs_f64();
            let outcome = if result.is_ok() { "success" } else { "error" };
            metrics::counter!(
                "kubimo_reconcile_total",
                "controller" => controller,
                "result" => outcome
            )
            .increment(1);
            metrics::histogram!(
                "kubimo_reconcile_duration_seconds",
                "controller" => controller
            )
            .record(elapsed);
            result
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct SomeModule;
    mod nested {
        pub struct WorkspaceReconciler;
    }

    #[test]
    fn controller_name_strips_module_path() {
        assert_eq!(controller_name::<SomeModule>(), "SomeModule");
        assert_eq!(
            controller_name::<nested::WorkspaceReconciler>(),
            "WorkspaceReconciler"
        );
    }

    #[tokio::test]
    async fn propagates_inner_service_result() {
        let mut svc =
            MetricsLayer::new("TestReconciler").layer(tower::service_fn(
                |(n, _): (Arc<u32>, Arc<()>)| async move {
                    if *n == 0 { Err("boom") } else { Ok(*n * 2) }
                },
            ));

        let ok = svc
            .ready()
            .await
            .unwrap()
            .call((Arc::new(21u32), Arc::new(())))
            .await;
        assert_eq!(ok, Ok(42));

        let err = svc
            .ready()
            .await
            .unwrap()
            .call((Arc::new(0u32), Arc::new(())))
            .await;
        assert_eq!(err, Err("boom"));
    }

    #[tokio::test]
    async fn records_reconcile_metrics_in_prometheus_format() {
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(([127, 0, 0, 1], 0))
            .install_recorder()
            .expect("failed to install test recorder");

        let mut ok_svc = MetricsLayer::new("TestOkReconciler").layer(tower::service_fn(
            |(_, _): (Arc<()>, Arc<()>)| async { Ok::<_, &'static str>(()) },
        ));
        ok_svc
            .ready()
            .await
            .unwrap()
            .call((Arc::new(()), Arc::new(())))
            .await
            .unwrap();

        let mut err_svc = MetricsLayer::new("TestErrReconciler").layer(tower::service_fn(
            |(_, _): (Arc<()>, Arc<()>)| async { Err::<(), _>("boom") },
        ));
        let _ = err_svc
            .ready()
            .await
            .unwrap()
            .call((Arc::new(()), Arc::new(())))
            .await;

        let rendered = handle.render();
        assert!(rendered.contains("kubimo_reconcile_total"));
        assert!(rendered.contains("kubimo_reconcile_duration_seconds"));
        assert!(rendered.contains(r#"controller="TestOkReconciler""#));
        assert!(rendered.contains(r#"controller="TestErrReconciler""#));
        assert!(rendered.contains(r#"result="success""#));
        assert!(rendered.contains(r#"result="error""#));
    }
}
