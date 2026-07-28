//! Reclaiming slots whose workspace is gone.
//!
//! A slot deliberately outlives the runner that used it: keeping it is what
//! makes reopening a workspace instant, since the next mount skips hydration
//! entirely. Nothing on the unpublish path is therefore allowed to delete one,
//! which leaves this as the only thing that ever does.
//!
//! Two properties matter more than promptness. A slot that is still published
//! is never touched, whatever its workspace's CR says — it is mounted into a
//! live pod. And an API error is never read as "the workspace is gone": losing
//! the API server for a minute must not delete a node's worth of tenant data.

use std::collections::HashSet;
use std::time::Duration;

use crate::clients::NamespacedClients;
use crate::store::SlotStore;

/// How often to sweep.
///
/// Slots are cheap to keep (a few MiB of real disk each, since the venv is
/// reflinked) and expensive to lose, so this is deliberately unhurried.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Sweep until the process exits.
pub async fn run(store: SlotStore, clients: NamespacedClients) {
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        match sweep(&store, &clients).await {
            Ok(0) => {}
            Ok(reclaimed) => tracing::info!(reclaimed, "reclaimed slots"),
            Err(err) => tracing::warn!(%err, "slot sweep failed"),
        }
    }
}

/// One pass. Returns how many slots were reclaimed.
async fn sweep(
    store: &SlotStore,
    clients: &NamespacedClients,
) -> Result<usize, crate::store::StoreError> {
    let workspaces = store.workspaces()?;
    if workspaces.is_empty() {
        return Ok(0);
    }
    let published = store.published_workspaces()?;
    let mut reclaimed = 0;
    for (workspace, namespace) in workspaces {
        if !should_reclaim(clients, &workspace, namespace.as_deref(), &published).await {
            continue;
        }
        match store.remove_slot(&workspace) {
            Ok(true) => {
                reclaimed += 1;
                tracing::info!(workspace, "reclaimed slot for deleted workspace");
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, workspace, "could not reclaim slot"),
        }
    }
    Ok(reclaimed)
}

/// Whether `workspace`'s slot can be dropped.
///
/// Deliberately biased towards keeping: every uncertain case — a published
/// slot, an unreachable API server, a workspace that still exists — returns
/// false. The cost of keeping a slot too long is disk; the cost of dropping one
/// too early is a tenant's unflushed work.
async fn should_reclaim(
    clients: &NamespacedClients,
    workspace: &str,
    namespace: Option<&str>,
    published: &HashSet<String>,
) -> bool {
    if published.contains(workspace) {
        return false;
    }
    // A Workspace CR is namespaced, and the agent's own namespace is not the
    // one it lives in. Without knowing which, a lookup returns "not found" for
    // a perfectly live workspace — and acting on that would delete a tenant's
    // slot. Slots recorded before the namespace was tracked have none, so they
    // are kept rather than guessed at.
    let Some(namespace) = namespace else {
        tracing::warn!(
            workspace,
            "no namespace recorded for this slot; keeping it rather than risking a live workspace"
        );
        return false;
    };
    let Some(client) = clients.get(namespace).await else {
        return false;
    };
    match client.api::<kubimo::Workspace>().get_opt(workspace).await {
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(err) => {
            tracing::warn!(%err, workspace, "could not check workspace; keeping its slot");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::SlotLayout;

    fn store() -> (tempfile::TempDir, SlotStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SlotStore::new(SlotLayout::new(dir.path()));
        (dir, store)
    }

    #[test]
    fn workspaces_lists_what_has_slots() {
        let (_dir, store) = store();
        assert!(store.workspaces().unwrap().is_empty());

        store.resolve_or_create("bmow-one", "platform").unwrap();
        store.resolve_or_create("bmow-two", "platform").unwrap();
        let mut found = store.workspaces().unwrap();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("bmow-one".to_string(), Some("platform".to_string())),
                ("bmow-two".to_string(), Some("platform".to_string())),
            ]
        );
    }

    #[test]
    fn removing_a_slot_clears_its_directory_and_index() {
        let (_dir, store) = store();
        let resolved = store.resolve_or_create("bmow-gone", "platform").unwrap();
        let slot_dir = store.layout().slot_dir(&resolved.id);
        assert!(slot_dir.is_dir());

        assert!(store.remove_slot("bmow-gone").unwrap());
        assert!(!slot_dir.exists());
        assert!(store.workspaces().unwrap().is_empty());
        // And the workspace looks brand new again, rather than pointing at a
        // slot that no longer exists.
        assert!(store.lookup("bmow-gone").unwrap().is_none());
    }

    /// The namespace has to survive in the index, because the reaper runs long
    /// after the mount that supplied it — and without it a Workspace lookup
    /// goes to the agent's own namespace, finds nothing, and reads as deleted.
    #[test]
    fn the_index_remembers_which_namespace_a_workspace_lives_in() {
        let (_dir, store) = store();
        store.resolve_or_create("bmow-ns", "some-tenant").unwrap();
        assert_eq!(
            store.workspaces().unwrap(),
            vec![("bmow-ns".to_string(), Some("some-tenant".to_string()))]
        );
        // And the slot itself still resolves: the namespace is a second line,
        // not something that corrupts the id on the first.
        assert!(store.lookup("bmow-ns").unwrap().is_some());
    }

    #[test]
    fn removing_an_unknown_slot_is_not_an_error() {
        let (_dir, store) = store();
        assert!(!store.remove_slot("bmow-absent").unwrap());
    }

    /// A published slot is mounted into a live pod, so the sweep has to see it
    /// as off-limits — reclaiming it would pull the filesystem out from under a
    /// running runner. This set is what `should_reclaim` checks first, before
    /// it ever asks the API server.
    #[test]
    fn a_slot_is_pinned_while_any_volume_is_published() {
        let (_dir, store) = store();
        let resolved = store.resolve_or_create("bmow-live", "platform").unwrap();
        assert!(!store.published_workspaces().unwrap().contains("bmow-live"));

        store
            .record_publish(
                "csi-abc",
                &crate::store::PublishedSlot {
                    workspace: "bmow-live".into(),
                    namespace: "platform".into(),
                    slot: resolved.id,
                    bucket: None,
                    key_prefix: None,
                },
            )
            .unwrap();
        assert!(store.published_workspaces().unwrap().contains("bmow-live"));

        store.forget_publish("csi-abc");
        assert!(!store.published_workspaces().unwrap().contains("bmow-live"));
    }
}
