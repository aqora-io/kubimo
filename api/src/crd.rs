use kube::{CustomResource, Resource};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::Display;
use url::Url;

use crate::validation::{
    runner_immutable_fields, runner_max_cpu_greater_than_min, runner_max_memory_greater_than_min,
    workspace_max_storage_greater_than_min,
};
use crate::{
    CpuQuantity, ResourceFactory, ResourceFactoryExt, ResourceNameExt, ResourceOwnerRefExt, Result,
    StorageQuantity,
};

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Requirement<T> {
    pub min: Option<T>,
    pub max: Option<T>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitRepo {
    pub url: String,
    pub branch: Option<String>,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitConfig {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct S3Request {
    pub url: Option<Url>,
    pub secret: Option<String>,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "aqora.io",
    version = "v1",
    kind = "KubimoWorkspace",
    shortname = "bmow",
    namespaced,
    validation = workspace_max_storage_greater_than_min(),
)]
#[serde(rename_all = "camelCase")]
pub struct KubimoWorkspaceSpec {
    pub storage: Option<Requirement<StorageQuantity>>,
    pub repo: Option<GitRepo>,
    pub git_config: Option<GitConfig>,
    pub ssh_key: Option<String>,
    pub s3_request: Option<S3Request>,
}

#[derive(Clone, Copy, Debug, Display)]
pub enum KubimoWorkspaceField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
}

impl ResourceFactory for KubimoWorkspace {
    fn new(name: &str, spec: Self::Spec) -> Self {
        Self::new(name, spec)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
pub enum KubimoRunnerCommand {
    #[default]
    Edit,
    Run,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "aqora.io",
    version = "v1",
    kind = "KubimoRunner",
    shortname = "bmor",
    selectable = ".spec.workspace",
    namespaced,
    validation = runner_immutable_fields(),
    validation = runner_max_memory_greater_than_min(),
    validation = runner_max_cpu_greater_than_min(),
)]
#[serde(rename_all = "camelCase")]
pub struct KubimoRunnerSpec {
    pub workspace: String,
    pub command: KubimoRunnerCommand,
    pub memory: Option<Requirement<StorageQuantity>>,
    pub cpu: Option<Requirement<CpuQuantity>>,
}

#[derive(Clone, Copy, Debug, Display)]
pub enum KubimoRunnerField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
    #[strum(serialize = "spec.workspace")]
    Workspace,
}

impl ResourceFactory for KubimoRunner {
    fn new(name: &str, spec: Self::Spec) -> Self {
        Self::new(name, spec)
    }
}

impl KubimoWorkspace {
    pub fn new_runner(&self, name: &str, spec: KubimoRunnerSpec) -> Result<KubimoRunner> {
        let mut runner = KubimoRunner::new(
            name,
            KubimoRunnerSpec {
                workspace: self.name()?.to_string(),
                ..spec
            },
        );
        runner
            .meta_mut()
            .owner_references
            .get_or_insert_default()
            .push(self.static_controller_owner_ref()?);
        Ok(runner)
    }

    pub fn create_runner(&self, spec: KubimoRunnerSpec) -> Result<KubimoRunner> {
        Ok(KubimoRunner::create(KubimoRunnerSpec {
            workspace: self.name()?.to_string(),
            ..spec
        }))
    }
}
