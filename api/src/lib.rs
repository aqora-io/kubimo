#[cfg(feature = "client")]
mod api;
#[cfg(feature = "client")]
mod client;
// Deliberately not behind `client`: these are the condition names consumers
// match on, and a consumer that only reads CRs takes this crate with
// `default-features = false`.
pub mod conditions;
mod crd;
// Same reasoning as `conditions`: the warm-pod-pool protocol strings are
// matched byte-exactly by the controller and the node agent.
mod error;
mod factory;
mod filter_params;
mod label;
#[cfg(feature = "client")]
mod list_stream;
mod manifest;
mod meta;
pub mod pool;
mod quantity;
mod secrets;
pub mod selector;
mod validation;

// Re-exported because the CRDs use them in their public types: a client cannot
// write `status.archive.lastSyncedAt` without constructing a `chrono` timestamp
// of the same version this crate was built against.
pub use chrono;
pub use json_patch_macros;
pub use k8s_openapi;
pub use kube;
pub use url;

#[cfg(feature = "client")]
pub use api::{Api, ApiListStream};
#[cfg(feature = "client")]
pub use client::{Client, ClientBuilder};
pub use crd::{
    Budget, BudgetResourceStatus, BudgetSpec, BudgetStatus, CacheJob, CacheJobField, CacheJobSpec,
    LogLevel, Pool, PoolSpec, PoolStatus, Requirement, Runner, RunnerClaim, RunnerCommand,
    RunnerField, RunnerIngress, RunnerLifecycle, RunnerSpec, RunnerStatus, RunnerTls, RunnerToken,
    StorageRequirement, Workspace, WorkspaceArchiveStatus, WorkspaceDir, WorkspaceDirContentUrl,
    WorkspaceDirDirectory, WorkspaceDirEntry, WorkspaceDirField, WorkspaceDirFile,
    WorkspaceDirMarimo, WorkspaceDirMarimoCache, WorkspaceDirSpec, WorkspaceDirSymlink,
    WorkspaceField, WorkspaceIndexer, WorkspaceIndexerPod, WorkspacePythonRuntime,
    WorkspaceRestoreFrom, WorkspaceRestoreSecrets, WorkspaceSlotStatus, WorkspaceSpec,
    WorkspaceStatus, WorkspaceStorageStatus, all_crds,
};
#[cfg(feature = "client")]
pub use error::ClientBuildError;
pub use error::{Error, Result};
pub use factory::ResourceFactory;
pub use filter_params::FilterParams;
pub use label::KubimoLabel;
#[cfg(feature = "client")]
pub use list_stream::{ApiListStreamExt, ListStream};
pub use manifest::{
    MANIFEST_FILE_NAME, ManifestDirectory, ManifestSecrets, ManifestVersion, SECRETS_FILE_NAME,
    WorkspaceManifest, build_manifest, manifest_url, secrets_url,
};
pub use meta::{ObjectMetaExt, ResourceNameExt, ResourceNamespaceExt, ResourceOwnerRefExt};
pub use quantity::{CpuQuantity, CpuUnit, Quantity, StorageQuantity, StorageUnit};
pub use secrets::{SecretEnvEntry, SecretFileEntry, WorkspaceSecrets, WorkspaceSecretsVersion};
pub use selector::{Expr, Expression, Selector, WellKnownField};

pub mod prelude {
    #[cfg(feature = "client")]
    pub use super::{ApiListStream, ApiListStreamExt};
    pub use super::{
        ObjectMetaExt, ResourceFactory, ResourceNameExt, ResourceNamespaceExt, ResourceOwnerRefExt,
    };
    pub use kube::{Resource, ResourceExt};
}
