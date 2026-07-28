//! Kubernetes clients scoped to a workspace's own namespace.
//!
//! A `Workspace` is namespaced, and the agent's namespace is not the one it
//! lives in: the agent runs beside the controller, while workspaces belong to
//! whoever created them. `Client::api::<T>()` resolves against the client's
//! default namespace, so a client built from the agent's own service account
//! looks in the wrong place — and a lookup that finds nothing is
//! indistinguishable from a workspace that has been deleted.
//!
//! That mattered everywhere: the final flush skipped as "workspace deleted",
//! the watcher stopping ten seconds after it started, `WorkspaceDirectory` CRs
//! written where the platform never reads them, and the reaper considering
//! every live slot reclaimable.
//!
//! kubelet supplies the right namespace with each `NodePublishVolume` because
//! the `CSIDriver` sets `podInfoOnMount`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

/// Cache of per-namespace clients, cheap to clone and shared between the CSI
/// plugin and the reaper.
#[derive(Clone, Default)]
pub struct NamespacedClients {
    /// `None` when the agent has no cluster access at all.
    enabled: bool,
    cache: Arc<Mutex<HashMap<String, kubimo::Client>>>,
}

impl NamespacedClients {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            cache: Default::default(),
        }
    }

    /// A client scoped to `namespace`.
    ///
    /// `None` when the agent has no cluster access, or when the scoped client
    /// cannot be built — never a client pointed at some other namespace, since
    /// callers read "not found" as "deleted".
    pub async fn get(&self, namespace: &str) -> Option<kubimo::Client> {
        if !self.enabled {
            return None;
        }
        let mut cache = self.cache.lock().await;
        if let Some(client) = cache.get(namespace) {
            return Some(client.clone());
        }
        // `kubimo-indexer`, matching the field manager the indexer writes
        // `WorkspaceDirectory` and `status.storage` under. Sharing the identity
        // keeps the two idempotent instead of conflicting under server-side
        // apply; it must stay distinct from the controller's own manager, which
        // owns `status.mode` on the same object.
        match kubimo::Client::builder()
            .name("kubimo-indexer")
            .namespace(namespace)
            .build()
            .await
        {
            Ok(client) => {
                cache.insert(namespace.to_string(), client.clone());
                Some(client)
            }
            Err(err) => {
                tracing::error!(%err, namespace, "could not build a client for this namespace");
                None
            }
        }
    }
}
