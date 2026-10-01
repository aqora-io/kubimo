use kubimo::k8s_openapi::api::networking::v1::Ingress;
use kubimo::{Runner, prelude::*};

use crate::context::Context;
use crate::controllers::ingress::{IngressParams, build_ingress, effective_ingress_path};
use crate::controllers::runner::apply_pod::runner_port;

use super::RunnerReconciler;

impl RunnerReconciler {
    pub(crate) async fn apply_ingress(
        &self,
        ctx: &Context,
        runner: &Runner,
    ) -> Result<Ingress, kubimo::Error> {
        let namespace = runner.require_namespace()?;
        let ingress = build_ingress(
            &ctx.config,
            IngressParams {
                name: runner.name()?,
                namespace: runner.metadata.namespace.clone(),
                owner_reference: runner.static_controller_owner_ref()?,
                path: effective_ingress_path(runner)?,
                service_name: runner.name()?,
                port: runner_port(runner),
                ingress: runner.spec.ingress.as_ref(),
            },
        );
        ctx.api_namespaced::<Ingress>(namespace)
            .patch(&ingress)
            .await
    }
}
