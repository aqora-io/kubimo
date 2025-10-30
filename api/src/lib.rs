#[cfg(feature = "client")]
mod api;
#[cfg(feature = "client")]
mod client;
mod crd;
mod error;
mod factory;
mod filter_params;
mod label;
#[cfg(feature = "client")]
mod list_stream;
mod meta;
mod quantity;
pub mod selector;
mod validation;

pub use json_patch_macros;
pub use k8s_openapi;
pub use kube;

#[cfg(feature = "client")]
pub use api::{Api, ApiListStream};
#[cfg(feature = "client")]
pub use client::{Client, ClientBuilder};
pub use crd::{
    Requirement, Runner, RunnerCommand, RunnerField, RunnerIngress, RunnerLifecycle, RunnerSpec,
    RunnerStatus, RunnerTls, Workspace, WorkspaceField, WorkspaceSpec, WorkspaceStatus, all_crds,
};
#[cfg(feature = "client")]
pub use error::ClientBuildError;
pub use error::{Error, Result};
pub use factory::ResourceFactory;
pub use filter_params::FilterParams;
pub use label::KubimoLabel;
#[cfg(feature = "client")]
pub use list_stream::{ApiListStreamExt, ListStream};
pub use meta::{ObjectMetaExt, ResourceNameExt, ResourceNamespaceExt, ResourceOwnerRefExt};
pub use quantity::{CpuQuantity, CpuUnit, Quantity, StorageQuantity, StorageUnit};
pub use selector::{Expr, WellKnownField};

pub mod prelude {
    #[cfg(feature = "client")]
    pub use super::{ApiListStream, ApiListStreamExt};
    pub use super::{
        ObjectMetaExt, ResourceFactory, ResourceNameExt, ResourceNamespaceExt, ResourceOwnerRefExt,
    };
    pub use kube::{Resource, ResourceExt};
}
