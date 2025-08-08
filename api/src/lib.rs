mod api;
mod client;
mod crd;
mod error;
mod factory;
mod filter_params;
mod label;
mod list_stream;
mod meta;
mod quantity;
mod selector;
mod validation;

pub use k8s_openapi;
pub use kube;

pub use api::Api;
pub use client::{Client, ClientBuilder};
pub use crd::{
    KubimoRunner, KubimoRunnerField, KubimoRunnerSpec, KubimoWorkspace, KubimoWorkspaceField,
    KubimoWorkspaceSpec, ReconciliationStatus,
};
pub use error::{ClientBuildError, Error, Result, StatusError};
pub use factory::{ResourceFactory, ResourceFactoryExt};
pub use filter_params::FilterParams;
pub use label::{KubimoLabel, ResourceLabelExt};
pub use list_stream::ApiListStreamExt;
pub use list_stream::ListStream;
pub use meta::{ObjectMetaExt, ResourceNameExt, ResourceOwnerRefExt};
pub use quantity::{CpuUnit, Quantity, StorageUnit};
pub use selector::{Expression, Selector};

pub mod prelude {
    pub use super::{
        ApiListStreamExt, ObjectMetaExt, ResourceFactory, ResourceFactoryExt, ResourceLabelExt,
        ResourceNameExt, ResourceOwnerRefExt,
    };
    pub use kube::{Resource, ResourceExt};
}
