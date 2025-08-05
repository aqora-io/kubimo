use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{CustomResource, CustomResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::service::ResourceFactory;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "aqora.io",
    version = "v1",
    kind = "KubimoWorkspace",
    namespaced
)]
pub struct KubimoWorkspaceSpec {}

impl ResourceFactory for KubimoWorkspace {
    fn new(name: &str, spec: KubimoWorkspaceSpec) -> Self {
        Self::new(name, spec)
    }
}

pub async fn apply<T>(
    crds: &kube::Api<CustomResourceDefinition>,
) -> Result<CustomResourceDefinition, kube::Error>
where
    T: CustomResourceExt,
{
    crds.patch(
        T::crd_name(),
        &kube::api::PatchParams::apply("kubimo"),
        &kube::api::Patch::Apply(T::crd()),
    )
    .await
}

pub async fn apply_all(client: &kube::Client) -> Result<(), kube::Error> {
    let crds = kube::Api::<CustomResourceDefinition>::all(client.clone());
    apply::<KubimoWorkspace>(&crds).await?;
    Ok(())
}
