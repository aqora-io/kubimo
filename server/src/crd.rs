use k8s_openapi::{
    apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
    apimachinery::pkg::api::resource::Quantity,
};
use kube::{CustomResource, CustomResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::id::ResourceFactory;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct StatusError {
    pub message: String,
    pub status: Option<String>,
    pub reason: Option<String>,
    pub code: Option<u16>,
}

impl StatusError {
    pub fn new(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            status: None,
            reason: None,
            code: None,
        }
    }
}

impl From<&kube::error::ErrorResponse> for StatusError {
    fn from(err: &kube::error::ErrorResponse) -> Self {
        Self {
            message: err.message.clone(),
            status: Some(err.status.clone()),
            reason: Some(err.reason.clone()),
            code: Some(err.code),
        }
    }
}

impl From<&kube::error::Error> for StatusError {
    fn from(err: &kube::error::Error) -> Self {
        match err {
            kube::error::Error::Api(e) => e.into(),
            _ => Self {
                message: err.to_string(),
                status: None,
                reason: None,
                code: None,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
pub struct KumimoWorkspaceStatus {
    pub reconciliation_error: Option<StatusError>,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "aqora.io",
    version = "v1",
    kind = "KubimoWorkspace",
    status = "KumimoWorkspaceStatus",
    namespaced
)]
pub struct KubimoWorkspaceSpec {
    pub storage: Quantity,
    pub storage_class_name: Option<String>,
}

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

#[cfg(test)]
mod tests {
    use kube::Resource;

    use super::*;

    #[test]
    fn test_crd() {
        let crd = KubimoWorkspace::new("workspace", KubimoWorkspaceSpec::default());
        println!("{:#?}", crd.owner_ref(&()));
    }
}
