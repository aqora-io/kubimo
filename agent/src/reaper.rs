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

use crate::store::SlotStore;

/// How often to sweep.
///
/// Slots are cheap to keep (a few MiB of real disk each, since the venv is
/// reflinked) and expensive to lose, so this is deliberately unhurried.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Sweep until the process exits.
pub async fn run(store: SlotStore, client: kubimo::Client) {
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        match sweep(&store, &client).await {
            Ok(0) => {}
            Ok(reclaimed) => tracing::info!(reclaimed, "reclaimed slots"),
            Err(err) => tracing::warn!(%err, "slot sweep failed"),
        }
    }
}

/// One pass. Returns how many slots were reclaimed.
async fn sweep(
    store: &SlotStore,
    client: &kubimo::Client,
) -> Result<usize, crate::store::StoreError> {
    let workspaces = store.workspaces()?;
    if workspaces.is_empty() {
        return Ok(0);
    }
    let published = store.published_workspaces()?;
    let mut reclaimed = 0;
    for workspace in workspaces {
        if !should_reclaim(client, &workspace, &published).await {
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
    client: &kubimo::Client,
    workspace: &str,
    published: &HashSet<String>,
) -> bool {
    if published.contains(workspace) {
        return false;
    }
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

        store.resolve_or_create("bmow-one").unwrap();
        store.resolve_or_create("bmow-two").unwrap();
        let mut found = store.workspaces().unwrap();
        found.sort();
        assert_eq!(found, vec!["bmow-one", "bmow-two"]);
    }

    #[test]
    fn removing_a_slot_clears_its_directory_and_index() {
        let (_dir, store) = store();
        let resolved = store.resolve_or_create("bmow-gone").unwrap();
        let slot_dir = store.layout().slot_dir(&resolved.id);
        assert!(slot_dir.is_dir());

        assert!(store.remove_slot("bmow-gone").unwrap());
        assert!(!slot_dir.exists());
        assert!(store.workspaces().unwrap().is_empty());
        // And the workspace looks brand new again, rather than pointing at a
        // slot that no longer exists.
        assert!(store.lookup("bmow-gone").unwrap().is_none());
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
        let resolved = store.resolve_or_create("bmow-live").unwrap();
        assert!(!store.published_workspaces().unwrap().contains("bmow-live"));

        store
            .record_publish(
                "csi-abc",
                &crate::store::PublishedSlot {
                    workspace: "bmow-live".into(),
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
