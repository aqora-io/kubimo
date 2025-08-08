use std::fmt::Debug;
use std::sync::Arc;

use kubimo::k8s_openapi::NamespaceResourceScope;
use kubimo::kube::{Resource, core::object::HasStatus, runtime::controller::Action};
use kubimo::{ReconciliationStatus, ResourceNameExt, StatusError};
use serde::{Serialize, de::DeserializeOwned};

use crate::context::Context;

pub trait UpdateStatus<E> {
    fn update_status(&mut self, error: Option<&E>);
}

impl<T, E> UpdateStatus<E> for T
where
    T: HasStatus<Status = ReconciliationStatus>,
    StatusError: for<'a> From<&'a E>,
{
    fn update_status(&mut self, error: Option<&E>) {
        *self.status_mut() = Some(ReconciliationStatus {
            reconciled: error.is_none(),
            reconciliation_error: error.map(StatusError::from),
        });
    }
}

pub async fn wrap_reconcile<T, E, F, Fut>(
    resource: Arc<T>,
    ctx: Arc<Context>,
    reconcile: F,
) -> Result<Action, E>
where
    F: FnOnce(Arc<T>, Arc<Context>) -> Fut,
    Fut: Future<Output = Result<Action, E>>,
    E: From<kubimo::Error>,
    T: Resource<Scope = NamespaceResourceScope>
        + UpdateStatus<E>
        + Serialize
        + DeserializeOwned
        + Clone
        + Debug
        + Send
        + 'static,
    <T as Resource>::DynamicType: Default,
{
    let ret = reconcile(resource.clone(), ctx.clone()).await;
    let api = ctx.client.api::<T>();
    let mut resource = api.get(resource.name()?).await?;
    resource.update_status(ret.as_ref().err());
    api.patch_status(&resource).await?;
    ret
}
