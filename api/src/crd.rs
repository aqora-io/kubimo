use kube::{CustomResource, Resource};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::validation::{
    runner_max_cpu_greater_than_min, runner_max_memory_greater_than_min,
    runner_workspace_immutable, workspace_max_storage_greater_than_min,
};
use crate::{
    CpuUnit, Quantity, ResourceFactory, ResourceFactoryExt, ResourceNameExt, ResourceOwnerRefExt,
    Result, StatusError, StorageUnit,
};

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReconciliationStatus {
    pub reconciled: bool,
    pub reconciliation_error: Option<StatusError>,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "aqora.io",
    version = "v1",
    kind = "KubimoWorkspace",
    status = "ReconciliationStatus",
    shortname = "bmow",
    selectable = ".status.reconciled",
    namespaced,
    validation = workspace_max_storage_greater_than_min(),
)]
#[serde(rename_all = "camelCase")]
pub struct KubimoWorkspaceSpec {
    pub min_storage: Option<Quantity<StorageUnit>>,
    pub max_storage: Option<Quantity<StorageUnit>>,
}

#[derive(Clone, Copy, Debug, Display)]
pub enum KubimoWorkspaceField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
    #[strum(serialize = "status.reconciled")]
    Reconciled,
}

impl ResourceFactory for KubimoWorkspace {
    fn new(name: &str, spec: Self::Spec) -> Self {
        Self::new(name, spec)
    }
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "aqora.io",
    version = "v1",
    kind = "KubimoRunner",
    status = "ReconciliationStatus",
    shortname = "bmor",
    selectable = ".status.reconciled",
    selectable = ".spec.workspace",
    namespaced,
    validation = runner_workspace_immutable(),
    validation = runner_max_memory_greater_than_min(),
    validation = runner_max_cpu_greater_than_min(),
)]
#[serde(rename_all = "camelCase")]
pub struct KubimoRunnerSpec {
    pub workspace: String,
    pub min_memory: Option<Quantity<StorageUnit>>,
    pub max_memory: Option<Quantity<StorageUnit>>,
    pub min_cpu: Option<Quantity<StorageUnit>>,
    pub max_cpu: Option<Quantity<CpuUnit>>,
}

#[derive(Clone, Copy, Debug, Display)]
pub enum KubimoRunnerField {
    #[strum(serialize = "metadata.name")]
    Name,
    #[strum(serialize = "metadata.namespace")]
    Namespace,
    #[strum(serialize = "status.reconciled")]
    Reconciled,
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
