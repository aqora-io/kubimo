//! Executing warm-pod claims on this node.
//!
//! The controller claims a warm pod by stamping a [`PoolClaim`] annotation on
//! it. Everything else about the claim is this agent's job: link the pod's
//! anonymous slot to the workspace, re-quota it, hydrate the workspace's files
//! from S3 into the directory marimo is already serving, start the sync
//! watcher, write the marker file the pod's `start.sh` is polling for, wait
//! for the pod to report the legacy-workspace migration that marker starts,
//! and ack by annotating the pod `claim-state: bound`. The controller withholds
//! the Service and Ingress until that ack, so no user ever reaches an
//! unhydrated workspace, or a notebook whose header is still to be written.
//!
//! Failure is always acked as `failed` rather than retried silently: the
//! controller deletes the pod and falls back to a cold start, and the pool
//! mints a replacement. Better one cold start than a pod bound to a slot in an
//! unknown state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use kubimo::k8s_openapi::api::core::v1::Pod;
use kubimo::kube::runtime::watcher::Event;
use kubimo::pool::{
    CLAIM_ANNOTATION, CLAIM_ERROR_ANNOTATION, CLAIM_MARKER_RELATIVE_PATH, CLAIM_STATE_ANNOTATION,
    CLAIM_STATE_BOUND, CLAIM_STATE_FAILED, MIGRATION_MARKER_RELATIVE_PATH, POOL_LABEL, PoolClaim,
};
use kubimo::{Expr, FilterParams, json_patch_macros::*};

use crate::csi::KubimoNode;

/// Watch this node's pool pods until the process exits.
///
/// Claims are handled one at a time: they are rare (one per notebook open),
/// hydration is the only slow step, and serialising them keeps every
/// slot-store interaction trivially ordered. The wait for the pod's migration
/// touches no slot-store state, so it runs beside the loop ([`PendingAcks`]).
/// The watcher relists on restart, which is what redelivers a claim the agent
/// crashed in the middle of.
pub async fn run(node: Arc<KubimoNode>, client: kubimo::Client) {
    let params = FilterParams::new()
        .with_fields(("spec.nodeName", node.node_id()))
        .with_labels(Expr::new(POOL_LABEL).exists());
    let pending = PendingAcks::default();
    loop {
        let mut stream = client.api_global::<Pod>().watch(&params);
        while let Some(event) = stream.next().await {
            let pod = match event {
                Ok(Event::Apply(pod) | Event::InitApply(pod)) => pod,
                Ok(_) => continue,
                Err(err) => {
                    tracing::warn!(%err, "pool pod watch error");
                    continue;
                }
            };
            if !wants_binding(&pod) {
                continue;
            }
            handle_claim(&node, &pod, &pending).await;
        }
        // The watcher only ends on repeated failures; back off and rebuild it.
        tracing::warn!("pool pod watch ended; restarting it");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// A pod with a claim annotation that has not reached a terminal claim state.
fn wants_binding(pod: &Pod) -> bool {
    if pod.metadata.deletion_timestamp.is_some() {
        return false;
    }
    let Some(annotations) = pod.metadata.annotations.as_ref() else {
        return false;
    };
    annotations.contains_key(CLAIM_ANNOTATION)
        && !matches!(
            annotations.get(CLAIM_STATE_ANNOTATION).map(String::as_str),
            Some(CLAIM_STATE_BOUND) | Some(CLAIM_STATE_FAILED)
        )
}

async fn handle_claim(node: &Arc<KubimoNode>, pod: &Pod, pending: &PendingAcks) {
    let (Some(pod_name), Some(pod_namespace), Some(pod_uid)) = (
        pod.metadata.name.as_deref(),
        pod.metadata.namespace.as_deref(),
        pod.metadata.uid.as_deref(),
    ) else {
        return;
    };
    let claim = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(CLAIM_ANNOTATION))
        .map(|raw| serde_json::from_str::<PoolClaim>(raw));
    let claim = match claim {
        Some(Ok(claim)) => claim,
        // Only the controller writes the annotation, so an unparseable one is
        // version skew to surface loudly, not input to tolerate.
        Some(Err(err)) => {
            tracing::error!(%err, pod = pod_name, "unparseable claim annotation");
            ack(
                node,
                pod_namespace,
                pod_name,
                Err("unparseable claim annotation"),
            )
            .await;
            return;
        }
        None => return,
    };
    match bind(node, pod_namespace, pod_name, pod_uid, &claim).await {
        Ok(slot_dir) => {
            let (node, namespace, name) =
                (node.clone(), pod_namespace.to_owned(), pod_name.to_owned());
            let workspace = claim.workspace;
            pending.spawn(slot_dir, pod_uid, move |migration| async move {
                if migration == Migration::TimedOut {
                    tracing::warn!(
                        pod = name.as_str(),
                        %workspace,
                        "the pod's migration did not report done in time; acking anyway"
                    );
                }
                tracing::info!(
                    pod = name.as_str(),
                    %workspace,
                    ?migration,
                    "bound a claimed pool slot"
                );
                ack(&node, &namespace, &name, Ok(())).await;
            });
        }
        Err(reason) => {
            tracing::warn!(
                pod = pod_name,
                workspace = %claim.workspace,
                reason,
                "claim failed; the controller will fall back to a cold start"
            );
            ack(node, pod_namespace, pod_name, Err(reason)).await;
        }
    }
}

/// Execute one claim end to end, returning the bound slot's directory. Any
/// `Err` is acked as `failed`.
async fn bind(
    node: &Arc<KubimoNode>,
    pod_namespace: &str,
    pod_name: &str,
    pod_uid: &str,
    claim: &PoolClaim,
) -> Result<PathBuf, &'static str> {
    let store = node.store();
    let workspace = claim.workspace.as_str();
    // Pool lock first, workspace lock second — the same order everywhere, so
    // the two lock families cannot deadlock.
    let pool_lock = store.lock_for_pool(pod_namespace, pod_name);
    let _pool_guard = pool_lock.lock().await;

    let pool_slot = match store.lookup_pool(pod_namespace, pod_name) {
        Ok(slot) => slot,
        Err(_) => return Err("could not read the pool slot index"),
    };
    let Some(pool_slot) = pool_slot else {
        // No anonymous slot. Either this claim already completed and the ack
        // was lost — re-ack it — or the slot genuinely never existed on this
        // node (an agent pod replacement destroyed the data volume).
        return already_bound_slot(node, pod_namespace, workspace, pod_uid)
            .ok_or("no anonymous slot for this pod on this node");
    };
    if pool_slot.pod_uid != pod_uid {
        return Err("the anonymous slot belongs to another incarnation of this pod");
    }

    let ws_lock = store.lock_for(pod_namespace, workspace);
    let _ws_guard = ws_lock.lock().await;

    // The workspace may already hold a slot on this node from an earlier
    // session. A flushed one is a cache — S3 has everything — so it is
    // superseded by the claim. An unflushed one may be the only copy of the
    // tenant's newest work and can never be destroyed for a warm start.
    match store.lookup(pod_namespace, workspace) {
        Ok(Some(_)) => {
            match store.is_published(pod_namespace, workspace) {
                Ok(false) => {}
                // The controller's eligibility check refuses workspaces with
                // live pods, so this is a race it lost; never adopt into it.
                Ok(true) => return Err("the workspace already has a published slot on this node"),
                Err(_) => return Err("could not check the workspace's publish state"),
            }
            match store.flushed_ago(pod_namespace, workspace) {
                Ok(Some(_)) => {
                    if store.remove_slot(pod_namespace, workspace).is_err() {
                        return Err("could not drop the workspace's superseded slot");
                    }
                }
                Ok(None) => return Err("the workspace has an unflushed slot on this node"),
                Err(_) => return Err("could not read the workspace's flush marker"),
            }
        }
        Ok(None) => {}
        Err(_) => return Err("could not look up the workspace's slot"),
    }

    let dir = store.layout().slot_dir(&pool_slot.id);
    // Re-quota to the workspace's own limit *before* hydration: the restore
    // checks free space against statvfs, which under a project quota reports
    // the quota itself.
    if let Some(limit_bytes) = claim.limit_bytes {
        match crate::quota::project_quota_enforced(store.layout().root()) {
            Ok(true) => {
                if crate::quota::set_project_limit(
                    store.layout().root(),
                    pool_slot.project_id,
                    limit_bytes,
                )
                .is_err()
                {
                    return Err("could not re-quota the slot");
                }
            }
            Ok(false) => {}
            Err(_) => return Err("could not check quota support"),
        }
    }

    // The pool's S3 secret was delivered when the anonymous volume was
    // published; the claim carries none. Losing it (an agent container
    // restart in between) must fail loudly — hydrating nothing and serving an
    // empty workspace as if it were the tenant's is the one unacceptable
    // outcome.
    let adopted_credentials =
        node.adopt_credentials(pod_namespace, pod_name, pod_namespace, workspace);
    if !adopted_credentials
        && node.s3_for(pod_namespace, workspace).is_none()
        && claim.bucket.is_some()
    {
        return Err("no S3 credentials held for this pod");
    }
    // Failures between here and the publish record must give back what
    // `adopt_credentials` took: the pod is doomed (the failed ack deletes
    // it) and its teardown only knows to forget the *pool* key, so a client
    // left under the workspace key would outlive it for the agent's
    // lifetime. Failures after the record need no hand-back — the unpublish
    // flush path uses the credentials and then forgets them itself.
    let forget_adopted = || {
        if adopted_credentials {
            node.forget_credentials(pod_namespace, workspace);
        }
    };
    let archive = claim
        .bucket
        .clone()
        .map(|bucket| crate::hydrate::ArchiveLocation {
            bucket,
            key_prefix: claim.key_prefix.clone(),
        });
    let seed = claim
        .seed_bucket
        .clone()
        .map(|bucket| crate::hydrate::SeedArchive {
            location: crate::hydrate::ArchiveLocation {
                bucket,
                key_prefix: claim.seed_key_prefix.clone(),
            },
            secrets: claim.seed_secrets.unwrap_or_default(),
        });
    // Hydrating into the live directory is safe: the restore writes file by
    // file and deletes nothing it does not know, marimo's file listing is
    // per-request, and no user session exists yet — the Service and Ingress
    // only appear after the ack.
    let s3 = node.s3_for(pod_namespace, workspace);
    let restored = node
        .hydrate_new_slot(
            workspace,
            &pool_slot.id,
            &dir,
            archive.as_ref(),
            seed.as_ref(),
            s3.as_ref(),
        )
        .await
        .map_err(|err| {
            tracing::warn!(err = %err.message(), workspace, "claim hydration failed");
            forget_adopted();
            "hydration failed"
        })?;
    if restored && crate::csi::chown_tree(&dir).is_err() {
        forget_adopted();
        return Err("could not chown the hydrated files");
    }

    // The publish record is written *before* adoption removes the pool link,
    // so at every instant an unpublish racing this claim finds at least one
    // of the two: the link (it then blocks on the pool lock and re-checks) or
    // the record (it runs the ordinary flush path). A window with neither
    // would let the teardown skip cleanup entirely, and the record written
    // just after would pin the workspace as published on this node forever —
    // blocking every future claim and skipping every final flush. Should
    // adoption fail below, the premature record is harmless: the controller
    // deletes the pod and the unpublish path consumes the record as usual.
    if store
        .record_publish(
            &pool_slot.volume_id,
            &crate::store::PublishedSlot {
                workspace: workspace.to_string(),
                namespace: pod_namespace.to_string(),
                slot: pool_slot.id.clone(),
                bucket: archive.as_ref().map(|archive| archive.bucket.clone()),
                key_prefix: archive
                    .as_ref()
                    .and_then(|archive| archive.key_prefix.clone()),
            },
        )
        .is_err()
    {
        // Same stance as the publish path: the mount works either way, only
        // the final flush would be skipped.
        tracing::warn!(
            workspace,
            "could not record the adopted publish; flush on stop will be skipped"
        );
    }
    // From here the slot is the workspace's: unpublish flushes it, the
    // reaper treats it as a warm cache, and the anonymous identity is gone.
    let adopted = match store.adopt_pool_slot(pod_namespace, pod_name, pod_namespace, workspace) {
        Ok(Some(adopted)) => adopted,
        Ok(None) => return Err("the anonymous slot vanished mid-claim"),
        Err(_) => return Err("could not adopt the slot"),
    };
    // No made-up quota: absent `limit_bytes` means the slot keeps the pool's
    // interim quota, whose exact value this claim does not know.
    node.publish_slot_status(
        workspace,
        pod_namespace,
        &adopted,
        claim.limit_bytes,
        archive.as_ref(),
    )
    .await;
    write_marker(&dir, pod_uid).map_err(|err| {
        tracing::warn!(%err, workspace, "could not write the claim marker");
        "could not write the claim marker"
    })?;
    if let Some(archive) = archive.as_ref() {
        node.start_watcher(
            &pool_slot.volume_id,
            pod_namespace,
            workspace,
            &dir,
            archive,
        )
        .await;
    }
    Ok(dir)
}

/// The slot this claim already bound before a restart or a lost ack: the
/// workspace resolves to a slot on this node whose marker names this pod.
///
/// The workspace is looked up in the pod's own namespace — that is the only
/// namespace [`bind`] ever adopts into. Deliberately does not restart the
/// watcher: the credentials were lost with the agent container, which is the
/// same (pre-existing) gap every published slot has across an agent restart.
fn already_bound_slot(
    node: &Arc<KubimoNode>,
    pod_namespace: &str,
    workspace: &str,
    pod_uid: &str,
) -> Option<PathBuf> {
    let Ok(Some(slot)) = node.store().lookup(pod_namespace, workspace) else {
        return None;
    };
    let dir = node.store().layout().slot_dir(&slot.id);
    std::fs::read_to_string(dir.join(CLAIM_MARKER_RELATIVE_PATH))
        .is_ok_and(|content| content.trim() == pod_uid)
        .then_some(dir)
}

/// Write the marker `start.sh` is polling for, atomically: create-and-rename
/// so a poll can never observe a half-written file. Root-owned inside a
/// root-owned directory, so the tenant cannot forge or truncate it.
fn write_marker(slot_dir: &std::path::Path, pod_uid: &str) -> std::io::Result<()> {
    let marker = slot_dir.join(CLAIM_MARKER_RELATIVE_PATH);
    let dir = marker.parent().expect("marker path has a parent");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("claimed.tmp");
    std::fs::write(&tmp, pod_uid)?;
    std::fs::rename(&tmp, &marker)?;
    Ok(())
}

/// How long an ack waits for the pod's migration. It reports within about
/// 150 ms of the claim marker, a legacy workspace's included, as the pod
/// prepares it while it waits for a claim; past this the ack goes out anyway,
/// as it did before pods reported, rather than hold the user on a migration
/// that is stuck.
const MIGRATION_WAIT: Duration = Duration::from_secs(30);
/// Every claim waits on at least one poll: the pod renames its report into
/// place, so a fine poll sees it as soon as it lands.
const MIGRATION_POLL: Duration = Duration::from_millis(50);

/// The pod's claim-time migration, as far as the ack is concerned.
#[derive(Debug, PartialEq)]
enum Migration {
    /// No report: an older image or controller, or a claim that beat
    /// `start.sh`, which then migrates before marimo serves anything.
    NotReported,
    /// Reported done after this long.
    Done(Duration),
    /// Still not done when the wait ran out.
    TimedOut,
}

/// Claims whose ack waits for the pod's migration report, by pod uid.
///
/// The wait runs beside the claim loop rather than in it: a pod that never
/// reports (deleted mid-claim, its migration process gone) would otherwise
/// hold every other claim on the node for [`MIGRATION_WAIT`]. One wait per
/// claim: watch events for the pod that were queued before its ack re-enter
/// [`handle_claim`] and join the wait already under way.
#[derive(Clone, Default)]
struct PendingAcks(Arc<Mutex<HashSet<String>>>);

impl PendingAcks {
    /// Run `ack` once the pod reports its migration or the wait runs out,
    /// without waiting for either. Returns whether a wait started, which it
    /// does not while this claim's ack is already waiting.
    fn spawn<F, Fut>(&self, slot_dir: PathBuf, pod_uid: &str, ack: F) -> bool
    where
        F: FnOnce(Migration) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if !self.lock().insert(pod_uid.to_owned()) {
            return false;
        }
        let pending = self.clone();
        let pod_uid = pod_uid.to_owned();
        tokio::spawn(async move {
            let migration = wait_for_migration(&slot_dir, &pod_uid, MIGRATION_WAIT).await;
            // Released before the ack, so an ack that fails (or panics) never
            // keeps a later event for this claim from waiting and acking again.
            pending.lock().remove(&pod_uid);
            ack(migration).await;
        });
        true
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        match self.0.lock() {
            Ok(waiting) => waiting,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Wait for the pod to report the migration the claim marker starts
/// ([`MIGRATION_MARKER_RELATIVE_PATH`] naming this pod), so the ack, and with
/// it the first session, never beats the notebook headers it writes.
async fn wait_for_migration(slot_dir: &Path, pod_uid: &str, timeout: Duration) -> Migration {
    let report = slot_dir.join(MIGRATION_MARKER_RELATIVE_PATH);
    let start = tokio::time::Instant::now();
    loop {
        match read_report(&report) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Migration::NotReported;
            }
            Ok(content) if content.trim() == pod_uid => return Migration::Done(start.elapsed()),
            // Pending, or not a report the pod wrote (a link, a FIFO): only
            // the pod naming this claim ends the wait early.
            _ => {}
        }
        if start.elapsed() >= timeout {
            return Migration::TimedOut;
        }
        tokio::time::sleep(MIGRATION_POLL).await;
    }
}

/// Read the report the way the pod leaves it. This agent runs as root and the
/// pod's user owns the file, so it follows no link, never blocks on a FIFO, and
/// reads no more than a pod uid needs.
fn read_report(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let mut content = String::new();
    file.take(64).read_to_string(&mut content)?;
    Ok(content)
}

/// Record the claim's outcome on the pod. Best-effort with one retry: a lost
/// ack is redelivered by the watcher's next relist, which re-enters the
/// idempotency probe and acks again.
async fn ack(node: &Arc<KubimoNode>, namespace: &str, pod_name: &str, outcome: Result<(), &str>) {
    let Some(client) = node.client_for(namespace).await else {
        tracing::warn!(pod = pod_name, "no client to ack the claim with");
        return;
    };
    let patch = match outcome {
        Ok(()) => {
            patch![add!(["metadata", "annotations", CLAIM_STATE_ANNOTATION] => CLAIM_STATE_BOUND),]
        }
        Err(reason) => patch![
            add!(["metadata", "annotations", CLAIM_STATE_ANNOTATION] => CLAIM_STATE_FAILED),
            add!(["metadata", "annotations", CLAIM_ERROR_ANNOTATION] => reason),
        ],
    };
    for attempt in 0..2 {
        match client
            .api_namespaced::<Pod>(namespace)
            .patch_json(pod_name, patch.clone())
            .await
        {
            Ok(_) => return,
            Err(err) if attempt == 0 => {
                tracing::warn!(%err, pod = pod_name, "could not ack the claim; retrying once");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(err) => {
                tracing::warn!(%err, pod = pod_name, "could not ack the claim; the next relist will retry");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn pod(annotations: &[(&str, &str)]) -> Pod {
        let mut pod = Pod::default();
        pod.metadata.name = Some("editors-a1b2c3d4".into());
        pod.metadata.annotations = Some(
            annotations
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        );
        pod
    }

    /// Only a pod with a claim and no terminal state needs binding; bound and
    /// failed pods are settled, and redelivering them (watch relists do) must
    /// not re-run the claim.
    #[test]
    fn only_unsettled_claims_want_binding() {
        assert!(wants_binding(&pod(&[(CLAIM_ANNOTATION, "{}")])));
        assert!(!wants_binding(&pod(&[])));
        assert!(!wants_binding(&pod(&[
            (CLAIM_ANNOTATION, "{}"),
            (CLAIM_STATE_ANNOTATION, CLAIM_STATE_BOUND),
        ])));
        assert!(!wants_binding(&pod(&[
            (CLAIM_ANNOTATION, "{}"),
            (CLAIM_STATE_ANNOTATION, CLAIM_STATE_FAILED),
        ])));
        let mut deleting = pod(&[(CLAIM_ANNOTATION, "{}")]);
        deleting.metadata.deletion_timestamp = Some(
            kubimo::k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                kubimo::k8s_openapi::jiff::Timestamp::UNIX_EPOCH,
            ),
        );
        assert!(!wants_binding(&deleting));
    }

    /// The marker appears atomically and names the pod, which is what the
    /// restart idempotency probe matches on.
    #[test]
    fn the_marker_is_written_atomically_and_names_the_pod() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), "uid-1").unwrap();
        let marker = dir.path().join(CLAIM_MARKER_RELATIVE_PATH);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "uid-1");
        assert!(!dir.path().join(".kubimo/claimed.tmp").exists());
        // Idempotent: a re-run replaces it in place.
        write_marker(dir.path(), "uid-1").unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "uid-1");
    }

    /// No report acks at once; a pending one holds the ack until the pod names
    /// this claim, and a report naming another pod never releases it.
    #[tokio::test]
    async fn the_ack_waits_for_the_pods_migration_report() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join(MIGRATION_MARKER_RELATIVE_PATH);
        assert_eq!(
            wait_for_migration(dir.path(), "uid-1", MIGRATION_WAIT).await,
            Migration::NotReported
        );

        std::fs::write(&report, "pending").unwrap();
        let pod = {
            let report = report.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                std::fs::write(&report, "uid-1\n").unwrap();
            })
        };
        assert!(matches!(
            wait_for_migration(dir.path(), "uid-1", MIGRATION_WAIT).await,
            Migration::Done(_)
        ));
        pod.await.unwrap();

        std::fs::write(&report, "uid-2").unwrap();
        assert_eq!(
            wait_for_migration(dir.path(), "uid-1", Duration::from_millis(200)).await,
            Migration::TimedOut
        );
    }

    /// The wait runs beside the claim loop: spawning it returns at once, a
    /// second event for the same claim joins the wait under way instead of
    /// starting another, and the one ack runs when the pod reports.
    #[tokio::test]
    async fn an_ack_waits_for_the_migration_beside_the_claim_loop() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join(MIGRATION_MARKER_RELATIVE_PATH);
        std::fs::write(&report, "pending").unwrap();
        let pending = PendingAcks::default();
        let (acked, mut acks) = tokio::sync::mpsc::unbounded_channel();
        let ack = |acked: tokio::sync::mpsc::UnboundedSender<Migration>| {
            move |migration| async move { acked.send(migration).unwrap() }
        };

        assert!(pending.spawn(dir.path().to_owned(), "uid-1", ack(acked.clone())));
        assert!(!pending.spawn(dir.path().to_owned(), "uid-1", ack(acked.clone())));
        let quiet = Duration::from_millis(200);
        assert!(tokio::time::timeout(quiet, acks.recv()).await.is_err());

        std::fs::write(&report, "uid-1").unwrap();
        let migration = tokio::time::timeout(Duration::from_secs(5), acks.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(migration, Migration::Done(_)));
        assert!(tokio::time::timeout(quiet, acks.recv()).await.is_err());
        // Settled: a later event for the claim waits (and acks) again.
        assert!(pending.spawn(dir.path().to_owned(), "uid-1", ack(acked)));
    }

    /// The pod's user owns the report: a link in its place is not followed,
    /// even to a file naming the claim.
    #[tokio::test]
    async fn a_linked_report_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, "uid-1").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join(MIGRATION_MARKER_RELATIVE_PATH))
            .unwrap();
        assert_eq!(
            wait_for_migration(dir.path(), "uid-1", Duration::from_millis(200)).await,
            Migration::TimedOut
        );
    }
}
