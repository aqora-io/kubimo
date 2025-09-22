use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{CustomResource, CustomResourceExt, Resource};
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

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
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
    group = "kubimo.aqora.io",
    version = "v1",
    kind = "Workspace",
    shortname = "bmow",
    namespaced,
    validation = workspace_max_storage_greater_than_min(),
)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSpec {
    pub storage: Option<Requirement<StorageQuantity>>,
    pub repo: Option<GitRepo>,
    pub git_config: Option<GitConfig>,
    pub ssh_key: Option<String>,
    pub s3_request: Option<S3Request>,
}

#[derive(Clone, Copy, Debug, Display)]
pub enum WorkspaceField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
}

impl ResourceFactory for Workspace {
    fn new(name: &str, spec: Self::Spec) -> Self {
        Self::new(name, spec)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
pub enum RunnerCommand {
    #[default]
    Edit,
    Run,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "kubimo.aqora.io",
    version = "v1",
    kind = "Runner",
    shortname = "bmor",
    selectable = ".spec.workspace",
    namespaced,
    validation = runner_immutable_fields(),
    validation = runner_max_memory_greater_than_min(),
    validation = runner_max_cpu_greater_than_min(),
)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSpec {
    pub workspace: String,
    pub command: RunnerCommand,
    pub memory: Option<Requirement<StorageQuantity>>,
    pub cpu: Option<Requirement<CpuQuantity>>,
}

#[derive(Clone, Copy, Debug, Display)]
pub enum RunnerField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
    #[strum(serialize = "spec.workspace")]
    Workspace,
}

impl ResourceFactory for Runner {
    fn new(name: &str, spec: Self::Spec) -> Self {
        Self::new(name, spec)
    }
}

impl Workspace {
    pub fn new_runner(&self, name: &str, spec: RunnerSpec) -> Result<Runner> {
        let mut runner = Runner::new(
            name,
            RunnerSpec {
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

    pub fn create_runner(&self, spec: RunnerSpec) -> Result<Runner> {
        Ok(Runner::create(RunnerSpec {
            workspace: self.name()?.to_string(),
            ..spec
        }))
    }
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "kubimo.aqora.io",
    version = "v1",
    kind = "Exporter",
    shortname = "bmoe",
    selectable = ".spec.workspace",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct ExporterSpec {
    pub workspace: String,
    pub s3_request: Option<S3Request>,
}

impl ResourceFactory for Exporter {
    fn new(name: &str, spec: Self::Spec) -> Self {
        Self::new(name, spec)
    }
}

impl Workspace {
    pub fn new_exporter(&self, name: &str, spec: ExporterSpec) -> Result<Exporter> {
        let mut exporter = Exporter::new(
            name,
            ExporterSpec {
                workspace: self.name()?.to_string(),
                ..spec
            },
        );
        exporter
            .meta_mut()
            .owner_references
            .get_or_insert_default()
            .push(self.static_controller_owner_ref()?);
        Ok(exporter)
    }

    pub fn create_exporter(&self, spec: ExporterSpec) -> Result<Exporter> {
        Ok(Exporter::create(ExporterSpec {
            workspace: self.name()?.to_string(),
            ..spec
        }))
    }
}

pub fn all_crds() -> Vec<CustomResourceDefinition> {
    vec![Workspace::crd(), Runner::crd(), Exporter::crd()]
}
