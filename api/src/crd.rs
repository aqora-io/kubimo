use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Container, EnvFromSource, EnvVar, Volume};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, CustomResourceExt, Resource};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::Display;
use url::Url;

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
pub struct WorkspaceIndexerPod {
    pub env: Option<Vec<EnvVar>>,
    pub env_from: Option<Vec<EnvFromSource>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceIndexer {
    /// Schedule in Cron format
    pub schedule: String,
    pub bucket: Option<String>,
    pub key_prefix: Option<String>,
    pub pod: Option<WorkspaceIndexerPod>,
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
    pub indexer: Option<WorkspaceIndexer>,
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

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default, Display)]
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
    selectable = ".spec.command",
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
    pub log_level: Option<String>,
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
    #[strum(serialize = "spec.command")]
    Command,
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

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "kubimo.aqora.io",
    version = "v1",
    kind = "CacheJob",
    shortname = "bmocj",
    selectable = ".spec.workspace",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct CacheJobSpec {
    pub workspace: String,
    pub log_level: Option<String>,
    pub memory: Option<Requirement<StorageQuantity>>,
    pub cpu: Option<Requirement<CpuQuantity>>,
    pub env: Option<Vec<EnvVar>>,
    pub env_from: Option<Vec<EnvFromSource>>,
    pub backoff_limit: Option<i32>,
}

#[derive(Clone, Copy, Debug, Display)]
pub enum CacheJobField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
    #[strum(serialize = "spec.workspace")]
    Workspace,
}

impl ResourceFactory for CacheJob {
    fn new(name: &str, spec: Self::Spec) -> Self {
        Self::new(name, spec)
    }
}

impl Workspace {
    pub fn new_cache_job(&self, name: &str, spec: CacheJobSpec) -> Result<CacheJob> {
        let mut cache_job = CacheJob::new(
            name,
            CacheJobSpec {
                workspace: self.name()?.to_string(),
                ..spec
            },
        );
        cache_job
            .meta_mut()
            .owner_references
            .get_or_insert_default()
            .push(self.static_controller_owner_ref()?);
        Ok(cache_job)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirDirectory {
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirSymlink {
    pub path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirContentUrl {
    pub url: Url,
    pub crc32: Option<u32>,
    pub e_tag: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirMarimoCache {
    pub format: String,
    pub size: Option<u64>,
    pub created: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
    pub url: Option<WorkspaceDirContentUrl>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirMarimo {
    pub meta_json: Option<WorkspaceDirContentUrl>,
    pub caches: Option<Vec<WorkspaceDirMarimoCache>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirFile {
    pub size: Option<u64>,
    pub marimo: Option<WorkspaceDirMarimo>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirEntry {
    pub name: String,
    pub created: Option<DateTime<Utc>>,
    pub modified: Option<DateTime<Utc>>,
    pub directory: Option<WorkspaceDirDirectory>,
    pub symlink: Option<WorkspaceDirSymlink>,
    pub file: Option<WorkspaceDirFile>,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "kubimo.aqora.io",
    version = "v1",
    kind = "WorkspaceDirectory",
    root = "WorkspaceDir",
    shortname = "bmowd",
    selectable = ".spec.workspace",
    selectable = ".spec.path",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDirSpec {
    pub workspace: String,
    pub path: String,
    pub entries: Option<Vec<WorkspaceDirEntry>>,
}

#[derive(Clone, Copy, Debug, Display)]
pub enum WorkspaceDirField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
    #[strum(serialize = "spec.workspace")]
    Workspace,
    #[strum(serialize = "spec.path")]
    Path,
}

impl ResourceFactory for WorkspaceDir {
    fn new(name: &str, spec: Self::Spec) -> Self {
        Self::new(name, spec)
    }
}

impl Workspace {
    pub fn new_workspace_directory(
        &self,
        name: &str,
        spec: WorkspaceDirSpec,
    ) -> Result<WorkspaceDir> {
        let mut workspace_directory = WorkspaceDir::new(
            name,
            WorkspaceDirSpec {
                workspace: self.name()?.to_string(),
                ..spec
            },
        );
        workspace_directory
            .meta_mut()
            .owner_references
            .get_or_insert_default()
            .push(self.static_controller_owner_ref()?);
        Ok(workspace_directory)
    }
}

pub fn all_crds() -> Vec<CustomResourceDefinition> {
    vec![
        Workspace::crd(),
        Runner::crd(),
        CacheJob::crd(),
        WorkspaceDir::crd(),
    ]
}
