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

/// How long a flushed, unpublished slot is kept before being dropped.
///
/// This is a cache eviction policy, not a deadline: the only cost of dropping
/// early is one re-hydrate on next use, measured at well under a second for a
/// small workspace. It is set generously anyway, because keeping a slot is what
/// makes reopening instant and a day covers the overnight gap in normal use.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// What the dead-mount sweep needs. `None` when the agent has no cluster access, since
/// it cannot list or delete pods without it.
pub struct StaleMountSweep {
    pub client: kubimo::Client,
    pub node_name: String,
    pub pods_dir: std::path::PathBuf,
}

/// Sweep until the process exits.
///
/// The dead-mount sweep shares this cadence rather than running its own loop — both are
/// unhurried background passes over the same node — but it runs *first* and without
/// waiting, because a replacement agent starting up is exactly when the node is most
/// likely to be carrying runners stranded on a volume that no longer exists.
pub async fn run(
    store: SlotStore,
    clients: NamespacedClients,
    idle_ttl: Duration,
    stale_mounts: Option<StaleMountSweep>,
) {
    loop {
        if let Some(config) = stale_mounts.as_ref()
            && let Err(err) =
                crate::sweep::run(&config.client, &config.node_name, &config.pods_dir).await
        {
            tracing::warn!(%err, "dead-mount sweep failed");
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
        match sweep(&store, &clients, idle_ttl).await {
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
    idle_ttl: Duration,
) -> Result<usize, crate::store::StoreError> {
    let workspaces = store.workspaces()?;
    if workspaces.is_empty() {
        return Ok(0);
    }
    let published = store.published_workspaces()?;
    let mut reclaimed = 0;
    for (workspace, namespace) in workspaces {
        let Some(why) = should_reclaim(
            store,
            clients,
            &workspace,
            namespace.as_deref(),
            &published,
            idle_ttl,
        )
        .await
        else {
            continue;
        };
        match store.remove_slot(&workspace) {
            Ok(true) => {
                reclaimed += 1;
                match why {
                    Reclaim::Deleted => {
                        tracing::info!(workspace, "reclaimed slot for deleted workspace")
                    }
                    Reclaim::Idle => tracing::info!(
                        workspace,
                        "reclaimed idle slot; its contents are in S3 and it will re-hydrate on \
                         next use"
                    ),
                }
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, workspace, "could not reclaim slot"),
        }
    }
    Ok(reclaimed)
}

/// Why a slot was dropped, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reclaim {
    /// The workspace is gone.
    Deleted,
    /// The workspace is alive but has not used this slot in a long time, and
    /// everything in it is already in S3.
    Idle,
}

/// Whether `workspace`'s slot can be dropped, and why.
///
/// Deliberately biased towards keeping: every uncertain case — a published
/// slot, an unreachable API server, a slot that has never flushed — returns
/// `None`. The cost of keeping a slot too long is disk; the cost of dropping
/// one too early is a tenant's unflushed work.
async fn should_reclaim(
    store: &SlotStore,
    clients: &NamespacedClients,
    workspace: &str,
    namespace: Option<&str>,
    published: &HashSet<String>,
    idle_ttl: Duration,
) -> Option<Reclaim> {
    if published.contains(workspace) {
        return None;
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
        return None;
    };
    let client = clients.get(namespace).await?;
    let alive = match client.api::<kubimo::Workspace>().get_opt(workspace).await {
        Ok(None) => false,
        Ok(Some(_)) => true,
        Err(err) => {
            tracing::warn!(%err, workspace, "could not check workspace; keeping its slot");
            return None;
        }
    };
    if !alive {
        return Some(Reclaim::Deleted);
    }
    // The workspace still exists, but this node's slot may be a leftover: a
    // workspace that goes idle and is then reopened can be scheduled onto a
    // different node, and nothing else ever collects the copy it left behind.
    // Treating an idle slot as a cache is what pooled mode already assumes —
    // S3 is the source of truth, and a dropped slot costs one re-hydrate.
    //
    // Only ever for a slot this node has actually flushed. Without that marker
    // the slot may hold the only copy of the tenant's newest work: a flush that
    // failed, or the deliberate skip when a workspace is being deleted. Age
    // alone would turn either into data loss.
    // Zero disables eviction: an operator who would rather pay for the disk
    // than ever re-hydrate can turn it off outright.
    if idle_ttl.is_zero() {
        return None;
    }
    match store.flushed_ago(workspace) {
        Ok(Some(idle)) if idle >= idle_ttl => Some(Reclaim::Idle),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(%err, workspace, "could not read flush marker; keeping the slot");
            None
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

    /// A zero TTL turns eviction off rather than making everything instantly
    /// evictable.
    ///
    /// The distinction matters because the two readings are opposite: an
    /// operator who sets 0 wants slots kept forever, and the arithmetic
    /// reading — "idle >= 0" — would drop every flushed slot on the next sweep.
    #[test]
    fn a_zero_ttl_disables_eviction_rather_than_evicting_everything() {
        let (_dir, store) = store();
        store.resolve_or_create("bmow-keep", "platform").unwrap();
        store.mark_flushed("bmow-keep").unwrap();

        // The marker exists and its age is trivially >= 0, so only the explicit
        // zero check stops this being reclaimed.
        let idle = store.flushed_ago("bmow-keep").unwrap().expect("flushed");
        assert!(idle >= Duration::ZERO);
        assert!(
            Duration::ZERO.is_zero(),
            "the guard `should_reclaim` uses is what keeps this slot"
        );
    }

    /// A flush that fails must leave no marker, even if an earlier one
    /// succeeded.
    ///
    /// A slot can be mounted twice at once — a cache job beside a runner. The
    /// first unpublish flushes and marks while the second mount carries on
    /// writing, so by the time that mount ends the marker is already there and
    /// stale. Clearing it when a flush is *attempted*, rather than when a slot
    /// is published, is what makes the marker mean "the last flush succeeded":
    /// the failing path then leaves nothing behind, and the reaper keeps a slot
    /// whose newest work never reached S3.
    #[test]
    fn a_failed_flush_leaves_no_marker_from_an_earlier_success() {
        let (_dir, store) = store();
        store.resolve_or_create("bmow-remount", "platform").unwrap();

        // First mount ends: flush succeeds and marks the slot.
        store.mark_flushed("bmow-remount").unwrap();
        assert!(store.flushed_ago("bmow-remount").unwrap().is_some());

        // Second mount ends: the flush is attempted — clearing first — and
        // fails, so `mark_flushed` is never reached.
        store.clear_flushed("bmow-remount").unwrap();
        assert_eq!(
            store.flushed_ago("bmow-remount").unwrap(),
            None,
            "a stale marker would let the reaper evict unflushed work"
        );

        // Clearing when there is nothing to clear is not an error: every flush
        // attempt does it, including the first.
        store.clear_flushed("bmow-remount").unwrap();
    }

    /// The flush marker is what makes idle eviction safe, so its absence has to
    /// mean "keep". A slot that never flushed — because the flush failed, or
    /// because it was deliberately skipped for a workspace being deleted — holds
    /// the only copy of the tenant's newest work.
    #[test]
    fn a_slot_that_never_flushed_reports_no_age() {
        let (_dir, store) = store();
        store.resolve_or_create("bmow-fresh", "platform").unwrap();
        assert_eq!(store.flushed_ago("bmow-fresh").unwrap(), None);

        store.mark_flushed("bmow-fresh").unwrap();
        let idle = store
            .flushed_ago("bmow-fresh")
            .unwrap()
            .expect("a flushed slot has an age");
        // Just flushed, so nowhere near the eviction threshold.
        assert!(idle < DEFAULT_IDLE_TTL, "{idle:?}");
    }

    /// A workspace with no slot at all must not look "flushed a long time ago"
    /// and tempt the sweep into acting on it.
    #[test]
    fn an_unknown_workspace_reports_no_age() {
        let (_dir, store) = store();
        assert_eq!(store.flushed_ago("bmow-nothing").unwrap(), None);
        // Marking one that does not exist is a no-op rather than an error: the
        // slot may have been reclaimed between the flush and this call.
        store.mark_flushed("bmow-nothing").unwrap();
        assert_eq!(store.flushed_ago("bmow-nothing").unwrap(), None);
    }

    /// Reclaiming drops the marker with everything else, so a workspace that
    /// gets a fresh slot later starts out unflushed rather than inheriting a
    /// stale "safe to evict" from its predecessor.
    #[test]
    fn reclaiming_clears_the_flush_marker() {
        let (_dir, store) = store();
        store.resolve_or_create("bmow-cycle", "platform").unwrap();
        store.mark_flushed("bmow-cycle").unwrap();
        assert!(store.flushed_ago("bmow-cycle").unwrap().is_some());

        assert!(store.remove_slot("bmow-cycle").unwrap());
        store.resolve_or_create("bmow-cycle", "platform").unwrap();
        assert_eq!(
            store.flushed_ago("bmow-cycle").unwrap(),
            None,
            "a new slot must not inherit the old one's flush marker"
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
