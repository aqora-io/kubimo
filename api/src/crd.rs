use kube::{CustomResource, Resource};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::validation::{
    runner_max_cpu_greater_than_min, runner_max_memory_greater_than_min,
    runner_workspace_immutable, workspace_max_storage_greater_than_min,
};
use crate::{
    CpuQuantity, ResourceFactory, ResourceFactoryExt, ResourceNameExt, ResourceOwnerRefExt, Result,
    StorageQuantity,
};

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
    pub min_storage: Option<StorageQuantity>,
    pub max_storage: Option<StorageQuantity>,
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

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, Default)]
#[kube(
    group = "aqora.io",
    version = "v1",
    kind = "KubimoRunner",
    shortname = "bmor",
    selectable = ".spec.workspace",
    namespaced,
    validation = runner_workspace_immutable(),
    validation = runner_max_memory_greater_than_min(),
    validation = runner_max_cpu_greater_than_min(),
)]
#[serde(rename_all = "camelCase")]
pub struct KubimoRunnerSpec {
    pub workspace: String,
    pub min_memory: Option<StorageQuantity>,
    pub max_memory: Option<StorageQuantity>,
    pub min_cpu: Option<CpuQuantity>,
    pub max_cpu: Option<CpuQuantity>,
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
