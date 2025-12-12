use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Container, EnvFromSource, EnvVar, Volume};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, CustomResourceExt, Resource};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::validation::{
    runner_immutable_fields, runner_max_cpu_greater_than_min, runner_max_memory_greater_than_min,
    workspace_max_storage_greater_than_min, workspace_no_volume_with_name,
};
use crate::{
    CpuQuantity, ResourceFactory, ResourceNameExt, ResourceOwnerRefExt, Result, StorageQuantity,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Requirement<T> {
    pub min: Option<T>,
    pub max: Option<T>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    pub conditions: Option<Vec<Condition>>,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "kubimo.aqora.io",
    version = "v1",
    kind = "Workspace",
    shortname = "bmow",
    namespaced,
    status = "WorkspaceStatus",
    validation = workspace_max_storage_greater_than_min(),
    validation = workspace_no_volume_with_name(),
)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSpec {
    pub storage: Option<Requirement<StorageQuantity>>,
    pub init_containers: Option<Vec<Container>>,
    #[schemars(length(max = 25))]
    pub volumes: Option<Vec<Volume>>,
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
#[serde(rename_all = "camelCase")]
pub struct RunnerTls {
    pub hosts: Option<Vec<String>>,
    pub cluster_issuer: Option<String>,
    pub secret_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunnerIngress {
    pub class_name: Option<String>,
    pub path: Option<String>,
    pub tls: Option<RunnerTls>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunnerLifecycle {
    pub delete_after_secs_inactive: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
pub enum RunnerCommand {
    #[default]
    Edit,
    Run,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunnerStatus {
    pub conditions: Option<Vec<Condition>>,
    pub last_active: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct RunnerToken {
    pub value: Option<String>,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "kubimo.aqora.io",
    version = "v1",
    kind = "Runner",
    shortname = "bmor",
    selectable = ".spec.workspace",
    namespaced,
    status = "RunnerStatus",
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
    pub env: Option<Vec<EnvVar>>,
    pub env_from: Option<Vec<EnvFromSource>>,
    pub ingress: Option<RunnerIngress>,
    pub lifecycle: Option<RunnerLifecycle>,
    pub token: Option<RunnerToken>,
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

impl Runner {
    pub fn ingress_tls_secret_name(&self) -> Result<String> {
        Ok(
            if let Some(secret_name) = self
                .spec
                .ingress
                .as_ref()
                .and_then(|ingress| ingress.tls.as_ref())
                .and_then(|tls| tls.secret_name.as_ref())
            {
                secret_name.clone()
            } else {
                format!("{}-tls", self.name()?)
            },
        )
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
}

pub fn all_crds() -> Vec<CustomResourceDefinition> {
    vec![Workspace::crd(), Runner::crd()]
}
