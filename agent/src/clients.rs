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

/// Field manager for everything the agent writes *as the indexer*:
/// `WorkspaceDirectory` objects and `status.storage`. Sharing the indexer's
/// identity keeps the two idempotent instead of conflicting under server-side
/// apply, and it must stay distinct from the controller's own manager, which
/// owns `status.mode` on the same object.
const INDEXER_MANAGER: &str = "kubimo-indexer";

/// Field manager for what only the agent knows: `status.slot` and
/// `status.archive`.
///
/// Deliberately *not* [`INDEXER_MANAGER`]. Under server-side apply a manager
/// owns exactly the fields its last apply contained, so omitting a field
/// relinquishes it — and the indexer's `status.storage` patches omit slot and
/// archive every time. Writing all three as one manager therefore had the
/// indexer silently delete the slot status a second after the agent wrote it,
/// with no error on either side. Separate managers own separate fields and
/// never overlap, so neither can revoke the other's.
const AGENT_MANAGER: &str = "kubimo-agent";

/// Cache of per-namespace clients, cheap to clone and shared between the CSI
/// plugin and the reaper.
#[derive(Clone, Default)]
pub struct NamespacedClients {
    /// `None` when the agent has no cluster access at all.
    enabled: bool,
    cache: Arc<Mutex<HashMap<(String, &'static str), kubimo::Client>>>,
}

impl NamespacedClients {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            cache: Default::default(),
        }
    }

    /// A client scoped to `namespace`, writing as the indexer.
    ///
    /// `None` when the agent has no cluster access, or when the scoped client
    /// cannot be built — never a client pointed at some other namespace, since
    /// callers read "not found" as "deleted".
    pub async fn get(&self, namespace: &str) -> Option<kubimo::Client> {
        self.get_as(namespace, INDEXER_MANAGER).await
    }

    /// A client scoped to `namespace` that writes the agent's own status
    /// fields, under a manager the indexer does not share.
    pub async fn get_for_slot_status(&self, namespace: &str) -> Option<kubimo::Client> {
        self.get_as(namespace, AGENT_MANAGER).await
    }

    async fn get_as(&self, namespace: &str, manager: &'static str) -> Option<kubimo::Client> {
        if !self.enabled {
            return None;
        }
        let key = (namespace.to_string(), manager);
        let mut cache = self.cache.lock().await;
        if let Some(client) = cache.get(&key) {
            return Some(client.clone());
        }
        match kubimo::Client::builder()
            .name(manager)
            .namespace(namespace)
            .build()
            .await
        {
            Ok(client) => {
                cache.insert(key, client.clone());
                Some(client)
            }
            Err(err) => {
                tracing::error!(
                    %err,
                    namespace,
                    manager,
                    "could not build a client for this namespace"
                );
                None
            }
        }
    }
}
