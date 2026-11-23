use kubimo::{Runner, prelude::*};

use crate::context::Context;
use crate::controllers::http_route::apply_http_route;
use crate::controllers::ingress::{RouteParams, apply_or_delete_ingress, effective_ingress_path};
use crate::controllers::runner::apply_pod::runner_port;

use super::RunnerReconciler;

impl RunnerReconciler {
    /// Converge the runner's routes: its Ingress (applied, or deleted once
    /// Ingresses are off) and, with a Gateway configured, its HTTPRoute. Both
    /// are attempted even if the other fails, so a cluster missing the Gateway
    /// API CRDs still keeps its Ingresses up to date.
    pub(crate) async fn apply_routes(
        &self,
        ctx: &Context,
        runner: &Runner,
    ) -> Result<(), kubimo::Error> {
        let namespace = runner.require_namespace()?;
        let params = RouteParams {
            name: runner.name()?,
            namespace: runner.metadata.namespace.clone(),
            owner_reference: runner.static_controller_owner_ref()?,
            path: effective_ingress_path(runner)?,
            service_name: runner.name()?,
            port: runner_port(runner),
            ingress: runner.spec.ingress.as_ref(),
        };
        let (ingress, http_route) = futures::join!(
            apply_or_delete_ingress(ctx, namespace, &params),
            apply_http_route(ctx, namespace, &params),
        );
        ingress.and(http_route)
    }
}
