//! The CSI node plugin.
//!
//! Slots reach runner pods as **inline ephemeral volumes** rather than a PVC
//! plus `subPath`. That choice is what keeps the normal Kubernetes scheduler in
//! charge: an inline ephemeral volume imposes no topology constraint, so
//! cluster-autoscaler scale-up, drain, cordon, taints and preemption all behave
//! normally. Pinning a pod to a node by hostname — which a per-node PVC would
//! force — makes scale-up impossible, because the autoscaler simulates against
//! a template node whose hostname never matches.
//!
//! It also sidesteps two other traps: kubelet's `fsGroup` recursion (this
//! driver advertises `fsGroupPolicy: None` and chowns exactly the slot) and the
//! immutability of `volumes`/`subPath` on an existing pod.
//!
//! Only the Identity and Node services exist. There is no controller service:
//! nothing is provisioned cluster-wide, so kubelet never calls `CreateVolume`.

use std::path::{Path, PathBuf};

use tonic::{Request, Response, Status};

use crate::quota;
use crate::store::SlotStore;

pub mod proto {
    #![allow(clippy::doc_overindented_list_items)]
    tonic::include_proto!("csi.v1");
}

use proto::identity_server::{Identity, IdentityServer};
use proto::node_server::{Node, NodeServer};

/// Must match the `CSIDriver` object's name and the directory the plugin
/// registers under in kubelet's plugin directory.
pub const DRIVER_NAME: &str = "kubimo.aqora.io";
const DRIVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Volume attribute naming the workspace whose slot to mount. Set by the
/// controller in the runner pod's inline volume definition.
const ATTR_WORKSPACE: &str = "workspace";
/// Volume attribute to declare how a workspace is installed. Set by the
/// controller in the runner pod's inline volume definition.
const ATTR_PYTHON_RUNTIME: &str = "python_runtime";
/// Optional per-slot hard capacity limit in bytes, from `spec.storage.max`.
const ATTR_LIMIT_BYTES: &str = "limitBytes";
/// Bucket and key prefix of the workspace's S3 archive, from `spec.indexer`.
/// Absent means "do not hydrate" — used by workspaces with no archive.
const ATTR_BUCKET: &str = "bucket";
const ATTR_KEY_PREFIX: &str = "keyPrefix";
/// Archive to seed a never-indexed workspace from, from `spec.restoreFrom`.
/// Read only when the workspace's own archive has no manifest, so a workspace
/// that has ever been flushed can never be overwritten by its seed.
const ATTR_SEED_BUCKET: &str = "seedBucket";
const ATTR_SEED_KEY_PREFIX: &str = "seedKeyPrefix";
/// How the seed's secrets are treated, from `spec.restoreFrom.secrets` —
/// `values` or `names-only`. Absent means names-only, the fail-safe default,
/// which is also what an agent older than this attribute does. A mode name,
/// never a credential, so it is safe on the Pod object.
const ATTR_SEED_SECRETS: &str = "seedSecrets";
/// Namespace of the pod being mounted, supplied by kubelet because the
/// `CSIDriver` sets `podInfoOnMount`.
///
/// The agent needs it because a Workspace CR is namespaced and does *not* live
/// in the agent's own namespace — the agent runs beside the controller, while
/// workspaces belong to whoever created them. Looking one up in the wrong
/// namespace returns "not found", which every caller here reads as "deleted".
const ATTR_POD_NAMESPACE: &str = "csi.storage.k8s.io/pod.namespace";

/// Owner of every slot's contents: the `me` user baked into the marimo image,
/// so the runner can write without kubelet's `fsGroup` recursion — which on a
/// shared volume would chown every slot on the node at every pod start.
pub(crate) const SLOT_UID: u32 = 1000;
pub(crate) const SLOT_GID: u32 = 1000;

pub struct KubimoIdentity;

#[tonic::async_trait]
impl Identity for KubimoIdentity {
    async fn get_plugin_info(
        &self,
        _request: Request<proto::GetPluginInfoRequest>,
    ) -> Result<Response<proto::GetPluginInfoResponse>, Status> {
        Ok(Response::new(proto::GetPluginInfoResponse {
            name: DRIVER_NAME.to_string(),
            vendor_version: DRIVER_VERSION.to_string(),
            manifest: Default::default(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<proto::GetPluginCapabilitiesRequest>,
    ) -> Result<Response<proto::GetPluginCapabilitiesResponse>, Status> {
        // No CONTROLLER_SERVICE and no VOLUME_ACCESSIBILITY_CONSTRAINTS: slots
        // are node-local and created on demand, and advertising topology would
        // reintroduce the scheduling constraint this design exists to avoid.
        Ok(Response::new(proto::GetPluginCapabilitiesResponse {
            capabilities: vec![],
        }))
    }

    async fn probe(
        &self,
        _request: Request<proto::ProbeRequest>,
    ) -> Result<Response<proto::ProbeResponse>, Status> {
        Ok(Response::new(proto::ProbeResponse { ready: Some(true) }))
    }
}

pub struct KubimoNode {
    node_id: String,
    store: SlotStore,
    default_limit_bytes: u64,
    /// Permit slots on a filesystem without project-quota enforcement.
    ///
    /// Off by default and deliberately explicit: an unquota'd slot has no
    /// capacity limit at all, so one workspace can fill the node volume and
    /// break every other tenant on that node. Silently degrading would be the
    /// easiest way to ship unlimited slots to production by accident.
    allow_unquotaed_slots: bool,
    /// Fallback client, built from the process environment (AWS_*).
    ///
    /// Only meaningful on a cluster where every workspace lives in the same S3
    /// account — a dev cluster, or a standalone kubimo. Anywhere the platform
    /// runs several environments side by side, each has its own bucket *and*
    /// endpoint, and the per-workspace credentials below are what actually
    /// serve them.
    s3: indexer::s3::S3Client,
    /// Whether [`Self::s3`] has any credentials at all, decided once at
    /// startup. `AmazonS3Builder::from_env` validates nothing, so without this
    /// an unconfigured fallback is indistinguishable from a working one until
    /// a request fails.
    has_env_credentials: bool,
    /// Per-`(namespace, workspace)` S3 clients, built from the credentials
    /// kubelet delivers with each `NodePublishVolume`.
    ///
    /// Keyed by [`KubimoNode::slot_key`] rather than the bare workspace name: a
    /// Workspace CR name is only unique within its namespace, so two
    /// same-named workspaces in different namespaces scheduled onto this node
    /// would otherwise share — and overwrite — each other's credentials.
    ///
    /// Retained past the publish because `NodeUnpublishVolume` carries no
    /// secrets and that is exactly when the final flush runs. Losing this on an
    /// agent *container* restart costs at most the final flush; an agent *pod*
    /// replacement destroys the whole node volume anyway, since it is a generic
    /// ephemeral volume.
    s3_clients: std::sync::Mutex<std::collections::HashMap<String, indexer::s3::S3Client>>,
    /// Kubernetes clients scoped to each workspace's own namespace. Everything
    /// downstream — the indexer's `WorkspaceDirectory` writes, `status.storage`
    /// patches, and every existence check — depends on getting this right.
    clients: crate::clients::NamespacedClients,
    /// Continuous sync task per *`(namespace, workspace)`*, not per published
    /// volume.
    ///
    /// A workspace can be published more than once on a node at the same time —
    /// a cache job is deliberately co-located with a live runner by the
    /// workspace affinity — and both mount the same slot directory. One watcher
    /// per volume would mean two walking and uploading the same tree with
    /// independent key sets, racing each other's `WorkspaceDirectory` writes and
    /// orphaning objects through the sweep. So they share one, and it lives
    /// until the last volume referencing it goes away.
    ///
    /// Keyed by [`KubimoNode::slot_key`], not the bare workspace name, for the
    /// same reason as [`Self::s3_clients`]: a Workspace CR name is only unique
    /// within its namespace.
    ///
    /// Only bound slots appear here: an idle slot has no runner and cannot
    /// change, so the watcher count on a node tracks running pods rather than
    /// total slots.
    watchers: std::sync::Mutex<std::collections::HashMap<String, Watcher>>,
}

/// A slot's sync task and the published volumes keeping it alive.
struct Watcher {
    handle: tokio::task::JoinHandle<()>,
    volumes: std::collections::HashSet<String>,
}

/// Give a restored tree to the runner's uid.
///
/// The agent writes as root, so without this the runner (uid 1000) cannot
/// modify its own files. `fsGroup` cannot do this job: on a shared node volume
/// kubelet would apply it to every slot on the node, not just this one.
fn chown_tree(dir: &Path) -> std::io::Result<()> {
    for entry in walkdir(dir)? {
        // `lchown`, never `chown`: `chown` follows symlinks, and this tree is
        // tenant-controlled. `restore` creates symlinks straight from the
        // manifest without validating their *targets* (only the link path is
        // checked), so a manifest entry pointing at `/etc/shadow` would have
        // this — running as node-root — hand that file to uid 1000.
        //
        // `lchown` also closes the TOCTOU: swapping a file for a symlink
        // between the walk and the chown changes nothing, because the symlink
        // itself is what gets chowned.
        std::os::unix::fs::lchown(&entry, Some(SLOT_UID), Some(SLOT_GID))?;
    }
    Ok(())
}

/// The `(false, false)` refusal: no project-quota enforcement and
/// `--allow-unquotaed-slots` not set. Shared between a freshly created slot
/// (rolled back by the caller) and an existing one (left in place, since it
/// may hold tenant data).
fn unquotaed_refusal(root: &Path) -> Status {
    Status::failed_precondition(format!(
        "{} is not mounted with project-quota enforcement (`prjquota`), so slots \
         would have no capacity limit. Mount the data volume with `prjquota`, or \
         pass --allow-unquotaed-slots to accept unlimited slots.",
        root.display()
    ))
}

/// Depth-first listing of `dir` and everything under it.
///
/// Deliberately does not follow symlinks: the tree contains tenant-controlled
/// paths, and following a planted link would chown something outside the slot.
fn walkdir(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = vec![dir.to_path_buf()];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path.clone());
            }
            found.push(path);
        }
    }
    Ok(found)
}

impl KubimoNode {
    pub fn new(
        node_id: String,
        store: SlotStore,
        default_limit_bytes: u64,
        allow_unquotaed_slots: bool,
        client: Option<kubimo::Client>,
    ) -> Self {
        Self {
            node_id,
            store,
            default_limit_bytes,
            allow_unquotaed_slots,
            s3: indexer::s3::S3Client::from_env(),
            has_env_credentials: std::env::var_os("AWS_ACCESS_KEY_ID").is_some(),
            s3_clients: Default::default(),
            clients: crate::clients::NamespacedClients::new(client.is_some()),
            watchers: Default::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &SlotStore {
        &self.store
    }

    async fn client_for(&self, namespace: &str) -> Option<kubimo::Client> {
        self.clients.get(namespace).await
    }

    /// The key `s3_clients` and `watchers` are indexed by.
    ///
    /// A Workspace CR name is only unique within its own namespace, so keying
    /// either map on the bare workspace name would let two identically-named
    /// workspaces in different namespaces, scheduled onto the same node,
    /// silently share — and overwrite — each other's credentials or watcher.
    fn slot_key(namespace: &str, workspace: &str) -> String {
        format!("{namespace}/{workspace}")
    }

    /// Remember the credentials kubelet delivered for `workspace` in `namespace`.
    fn remember_credentials(
        &self,
        namespace: &str,
        workspace: &str,
        secrets: &std::collections::HashMap<String, String>,
    ) {
        if secrets.is_empty() {
            return;
        }
        let client = indexer::s3::S3Client::from_options(secrets.iter());
        self.lock_s3_clients()
            .insert(Self::slot_key(namespace, workspace), client);
    }

    /// The S3 client to use for `workspace` in `namespace`.
    ///
    /// `None` means there is nowhere to read or write this workspace's archive:
    /// no credentials arrived with the mount and the agent has none of its own.
    /// Callers must say so rather than proceeding, because the failure is
    /// otherwise invisible — a hydrate looks like an empty workspace and a
    /// flush is logged and dropped.
    fn s3_for(&self, namespace: &str, workspace: &str) -> Option<indexer::s3::S3Client> {
        if let Some(client) = self
            .lock_s3_clients()
            .get(&Self::slot_key(namespace, workspace))
        {
            return Some(client.clone());
        }
        self.has_env_credentials.then(|| self.s3.clone())
    }

    fn forget_credentials(&self, namespace: &str, workspace: &str) {
        self.lock_s3_clients()
            .remove(&Self::slot_key(namespace, workspace));
    }

    fn lock_s3_clients(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, indexer::s3::S3Client>> {
        match self.s3_clients.lock() {
            Ok(clients) => clients,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Start continuous sync for a freshly published slot.
    async fn start_watcher(
        &self,
        volume_id: &str,
        namespace: &str,
        workspace: &str,
        dir: &Path,
        archive: &crate::hydrate::ArchiveLocation,
    ) {
        let Some(client) = self.client_for(namespace).await else {
            return;
        };
        let Some(s3) = self.s3_for(namespace, workspace) else {
            tracing::warn!(
                workspace,
                "no S3 credentials for this workspace; its slot will not be synced"
            );
            return;
        };
        let key = Self::slot_key(namespace, workspace);
        // Deliberately scoped: `spawn_watcher` awaits (it reads the workspace's
        // existing directory CRs to recover its key layout), and this is a std
        // mutex whose guard cannot be held across that.
        {
            let mut watchers = self.lock_watchers();
            if let Some(watcher) = watchers.get_mut(&key) {
                watcher.volumes.insert(volume_id.to_string());
                return;
            }
        }
        let handle = match crate::hydrate::spawn_watcher(
            dir,
            workspace,
            archive,
            client.clone(),
            s3,
        )
        .await
        {
            Ok(handle) => handle,
            Err(err) => {
                tracing::warn!(%err, workspace, "could not start watcher; \
                     the slot will only be flushed when its runner stops");
                return;
            }
        };
        let mut watchers = self.lock_watchers();
        // Re-check: dropping the lock above leaves room for another publish of
        // the same workspace to have won the race. Keep the incumbent, since two
        // watchers on one slot would upload it twice over.
        if let Some(watcher) = watchers.get_mut(&key) {
            watcher.volumes.insert(volume_id.to_string());
            handle.abort();
            return;
        }
        watchers.insert(
            key,
            Watcher {
                handle,
                volumes: std::iter::once(volume_id.to_string()).collect(),
            },
        );
        tracing::info!(workspace, "watching slot for changes");
    }

    fn lock_watchers(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Watcher>> {
        match self.watchers.lock() {
            Ok(watchers) => watchers,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Release one volume's claim on a workspace's watcher.
    ///
    /// The watcher is aborted rather than gracefully stopped once the last
    /// claim goes: a final flush follows immediately, so anything it was midway
    /// through is redone.
    fn stop_watcher(&self, volume_id: &str, namespace: &str, workspace: &str) {
        let key = Self::slot_key(namespace, workspace);
        let mut watchers = self.lock_watchers();
        let Some(watcher) = watchers.get_mut(&key) else {
            return;
        };
        watcher.volumes.remove(volume_id);
        if watcher.volumes.is_empty()
            && let Some(watcher) = watchers.remove(&key)
        {
            watcher.handle.abort();
        }
    }

    /// Whether the workspace this slot belongs to is deleted or on its way out.
    ///
    /// Deleting a workspace tears down its runners, and the resulting unpublish
    /// would otherwise flush the slot straight back into the S3 prefix the
    /// platform had just purged — recreating a full copy of data that was meant
    /// to be erased, with no CR left to find it by. The dedicated path has no
    /// equivalent, because its indexer pod does not upload on SIGTERM.
    ///
    /// Errors count as "still there": failing to reach the API server must not
    /// turn into silently dropping a legitimate flush.
    async fn workspace_is_going_away(&self, client: &kubimo::Client, workspace: &str) -> bool {
        match client.api::<kubimo::Workspace>().get_opt(workspace).await {
            Ok(Some(found)) => found.metadata.deletion_timestamp.is_some(),
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(%err, workspace, "could not check whether the workspace still exists; flushing anyway");
                false
            }
        }
    }

    /// Push a published slot's tracked files to S3.
    ///
    /// Never fails the unpublish. The slot keeps its data on disk either way, so
    /// a failed flush is recoverable — whereas refusing to unmount would leave
    /// the pod stuck Terminating and block the node from draining.
    async fn flush_published_slot(&self, published: &crate::store::PublishedSlot) {
        // A record written before the namespace was part of one, or one whose
        // namespace line was lost. Every lookup below is namespaced, and an
        // empty namespace builds a client that asks for `/namespaces//...`:
        // the 404 that comes back reads as "workspace deleted" and the flush is
        // skipped with a log line saying so, which is a lie.
        if published.namespace.is_empty() {
            tracing::error!(
                workspace = %published.workspace,
                slot = %published.slot,
                "publish record has no namespace, so its final flush is being skipped; \
                 changes since the last sync exist only on this node"
            );
            return;
        }
        // No bucket recorded means the workspace has no archive configured;
        // there is nowhere to flush to.
        let Some(bucket) = published.bucket.clone() else {
            return;
        };
        let archive = crate::hydrate::ArchiveLocation {
            bucket,
            key_prefix: published.key_prefix.clone(),
        };
        let workspace = published.workspace.as_str();
        let Some(client) = self.client_for(&published.namespace).await else {
            // Without cluster access there is no way to refresh the directory
            // CRs, and the upload path needs one. Say so: this is the last
            // chance to persist whatever the watcher had not already synced.
            tracing::error!(
                workspace,
                namespace = %published.namespace,
                "no Kubernetes access for this workspace, so its final flush is being skipped; \
                 changes since the last sync exist only on this node"
            );
            return;
        };
        if self.workspace_is_going_away(&client, workspace).await {
            tracing::info!(
                workspace,
                "workspace is deleted or terminating; skipping flush"
            );
            return;
        }
        // `NodeUnpublishVolume` carries no secrets, so this is the credential
        // set remembered when the volume was published. Losing it — an agent
        // container restart between publish and unpublish — must be loud: the
        // slot's newest work is on disk but cannot be persisted.
        let Some(s3) = self.s3_for(&published.namespace, workspace) else {
            tracing::error!(
                workspace,
                "no S3 credentials for this workspace, so its final flush is being skipped; \
                 changes since the last sync exist only on this node"
            );
            return;
        };
        let dir = self.store.layout().slot_dir(&published.slot);
        tracing::info!(workspace, slot = %published.slot, "flushing slot to S3");
        // Drop any previous marker *before* attempting, so failing here can
        // never leave a stale one behind. A slot can be mounted twice at once —
        // a cache job beside a runner — and the first unpublish marks it while
        // the second mount carries on writing; if that mount's own flush then
        // failed, an untouched marker would read as permission to evict work
        // that never reached S3.
        if let Err(err) = self.store.clear_flushed(&published.namespace, workspace) {
            tracing::warn!(%err, workspace, "could not clear the flush marker");
        }
        let flushed =
            match crate::hydrate::flush_slot(&dir, workspace, &archive, &client, &s3).await {
                Ok(Some(flushed)) => flushed,
                // The upload pipeline reports what it could not write rather than
                // failing: a file whose upload failed, a walk that came back empty,
                // a manifest that never landed. Every one of those leaves the
                // archive short of the slot, so it is not a durability boundary and
                // must not be recorded as one.
                //
                // A slot with no `workspace` subdirectory lands here too and so
                // never gets an idle-eviction marker. That is deliberate: it is
                // still reclaimed by the workspace-deleted path, and inventing a
                // marker for a tree nothing has ever walked is exactly the claim
                // this is here to stop making.
                Ok(None) => {
                    tracing::error!(
                        workspace,
                        slot = %published.slot,
                        "flush did not complete, so the slot is being kept; changes since the \
                         last sync exist only on this node"
                    );
                    return;
                }
                Err(err) => {
                    tracing::error!(%err, workspace, "flush failed; slot data is still on disk");
                    return;
                }
            };
        // Only now is the slot safe to treat as a cache. The reaper refuses to
        // evict a slot without this marker, precisely so a failed flush — or
        // the deliberate skip above when a workspace is being deleted — keeps
        // the only copy of the tenant's newest work on disk.
        if let Err(err) = self.store.mark_flushed(&published.namespace, workspace) {
            tracing::warn!(
                %err,
                workspace,
                "could not record the flush; the slot will be kept rather than evicted"
            );
        }
        // `status.archive` is not written here. The flush runs `indexer::upload`,
        // which records the sync itself on any batch that lands cleanly — the same
        // path the watcher takes. Writing it again from here under a second field
        // manager would put two owners on one field and make every flush a 409.
        tracing::info!(
            workspace,
            slot = %published.slot,
            content_bytes = flushed.content_bytes,
            "flushed slot to its archive"
        );
    }

    /// Record where a pooled workspace actually lives, on the CR.
    ///
    /// Without this, the only way to find out which node holds a workspace's
    /// slot, what its id is, or where its archive sits, is to read agent logs on
    /// every node — which is exactly what debugging pooled mode has meant so
    /// far. Best-effort: this is diagnostics, and failing it must never fail a
    /// mount that is otherwise fine.
    ///
    /// `lastSyncedAt` is deliberately not set here. It means "this content
    /// reached S3", which is only true after a flush, and claiming it at publish
    /// time would make an unflushed slot look durable.
    async fn publish_slot_status(
        &self,
        workspace: &str,
        namespace: &str,
        slot: &crate::store::ResolvedSlot,
        limit_bytes: u64,
        archive: Option<&crate::hydrate::ArchiveLocation>,
    ) {
        // A manager of its own, not the indexer's. Server-side apply gives a
        // manager exactly the fields its last apply contained, so the indexer's
        // `status.storage` patches — which never mention slot or archive —
        // would relinquish them, and the API server would drop what was written
        // here moments earlier. No error is reported on either side; the fields
        // simply stay empty, which is what they did.
        let Some(client) = self.clients.get_for_slot_status(namespace).await else {
            return;
        };
        let mut patch = kubimo::Workspace::new(workspace, Default::default());
        patch.status = Some(kubimo::WorkspaceStatus {
            slot: Some(kubimo::WorkspaceSlotStatus {
                node: Some(self.node_id.clone()),
                id: Some(slot.id.to_string()),
                quota: Some(indexer::disk::storage_quantity(limit_bytes)),
            }),
            archive: archive.map(|archive| kubimo::WorkspaceArchiveStatus {
                key_prefix: archive.key_prefix.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
        if let Err(err) = client.api::<kubimo::Workspace>().patch_status(&patch).await {
            tracing::warn!(%err, workspace, "could not record slot status");
        }
    }

    /// Fill a freshly created slot from the workspace's own archive, falling
    /// back to its seed.
    ///
    /// Returns whether anything was written. Split out from [`Self::prepare_slot`]
    /// so its failure can be handled in one place: a half-hydrated slot has to be
    /// discarded, or the retry inherits it and publishes it empty.
    async fn hydrate_new_slot(
        &self,
        workspace: &str,
        slot: &crate::slot::SlotId,
        dir: &Path,
        archive: Option<&crate::hydrate::ArchiveLocation>,
        seed: Option<&crate::hydrate::SeedArchive>,
        s3: Option<&indexer::s3::S3Client>,
    ) -> Result<bool, Status> {
        let Some(s3) = s3 else {
            return Ok(false);
        };
        let mut restored = false;
        if let Some(archive) = archive {
            // Always `Values`: this is the workspace's *own* archive, so a warm
            // reopen must get its own `.env` back, not placeholders.
            restored = crate::hydrate::hydrate_slot(
                dir,
                archive,
                s3,
                kubimo::WorkspaceRestoreSecrets::Values,
            )
            .await
            .map_err(|err| Status::internal(format!("hydrating slot: {err}")))?;
            tracing::info!(workspace, slot = %slot, hydrated = restored, "slot hydrated");
        }
        // Fall back to the seed only when the workspace's own archive had no
        // manifest, which is the existing signal for "never indexed". The
        // workspace's own content always wins: re-seeding a warm workspace —
        // one whose slot was reclaimed, or whose agent pod was replaced — would
        // overwrite the tenant's work with the template it started from.
        //
        // Note this uses the *workspace's* client, not the agent's own. The
        // agent has no credentials of its own since they became per-workspace,
        // so reading the seed through `self.s3` sent it to the instance
        // metadata service looking for some, and every clone failed to mount.
        if !restored && let Some(seed) = seed {
            let seeded = crate::hydrate::hydrate_slot(dir, &seed.location, s3, seed.secrets)
                .await
                .map_err(|err| Status::internal(format!("seeding slot: {err}")))?;
            tracing::info!(workspace, slot = %slot, seeded, "slot seeded");
            restored |= seeded;
        }
        Ok(restored)
    }

    /// Resolve the slot for `workspace`, provisioning it on first creation, and
    /// return it together with its directory.
    ///
    /// The returned [`crate::store::ResolvedSlot`] is authoritative: the caller
    /// must not look the slot up again, since a concurrent first publish could
    /// otherwise observe a *different* slot between the two reads — the very
    /// split that would bind-mount one directory while the publish record named
    /// another. Callers therefore hold [`SlotStore::lock_for`] across this.
    async fn prepare_slot(
        &self,
        namespace: &str,
        workspace: &str,
        limit_bytes: u64,
        archive: Option<&crate::hydrate::ArchiveLocation>,
        seed: Option<&crate::hydrate::SeedArchive>,
        python_runtime: Option<&str>,
    ) -> Result<(crate::store::ResolvedSlot, PathBuf), Status> {
        let resolved = self
            .store
            .resolve_or_create(namespace, workspace)
            .map_err(|err| Status::internal(format!("resolving slot: {err}")))?;
        let dir = self.store.layout().slot_dir(&resolved.id);
        let quotas_enforced = quota::project_quota_enforced(self.store.layout().root())
            .map_err(|err| Status::internal(format!("checking quota support: {err}")))?;

        if resolved.created {
            if let Err(err) = self
                .provision_new_slot(
                    namespace,
                    workspace,
                    &resolved,
                    &dir,
                    limit_bytes,
                    quotas_enforced,
                    archive,
                    seed,
                    python_runtime,
                )
                .await
            {
                // A slot that failed provisioning must not survive: kubelet's retry finds
                // `created: false` and skips quota, ownership and hydration entirely, so a
                // transient error here would otherwise become a permanently unquotaed,
                // never-hydrated slot. Dropping it makes the retry start over.
                if let Err(remove_err) = self.store.remove_slot(namespace, workspace) {
                    tracing::error!(%remove_err, workspace, slot = %resolved.id,
                        "could not drop a slot whose provisioning failed; a retry will reuse it as-is");
                }
                return Err(err);
            }
        } else if quotas_enforced {
            // Re-applied on every publish, not just creation: slot capacity changes
            // arrive as a new `limitBytes` volume attribute on the next publish (see
            // `node_expand_volume`), and skipping existing slots would silently discard
            // them.
            quota::set_project_limit(self.store.layout().root(), resolved.project_id, limit_bytes)
                .map_err(|err| Status::internal(format!("setting quota: {err}")))?;
        } else if !self.allow_unquotaed_slots {
            // Unlike the created-slot case, there is nothing to roll back here:
            // the slot already exists and may hold tenant data, so refusing the
            // publish must leave it in place rather than removing it.
            return Err(unquotaed_refusal(self.store.layout().root()));
        }
        // else: quotas unenforced but `--allow-unquotaed-slots` is set, on an
        // already-existing slot — the warning for this only fires on
        // creation, to avoid repeating it on every publish.

        Ok((resolved, dir))
    }

    /// Provision a freshly created slot: project quota, ownership, the venv
    /// template, then hydration from S3.
    ///
    /// Split out from [`Self::prepare_slot`] so that a failure at any step is
    /// rolled back in one place. Every step here runs only once, when the slot
    /// is created; a half-provisioned slot left behind would be published as-is
    /// on retry — unquotaed and empty — so the caller discards it on error.
    #[allow(clippy::too_many_arguments)]
    async fn provision_new_slot(
        &self,
        namespace: &str,
        workspace: &str,
        resolved: &crate::store::ResolvedSlot,
        dir: &Path,
        limit_bytes: u64,
        quotas_enforced: bool,
        archive: Option<&crate::hydrate::ArchiveLocation>,
        seed: Option<&crate::hydrate::SeedArchive>,
        python_runtime: Option<&str>,
    ) -> Result<(), Status> {
        match (quotas_enforced, self.allow_unquotaed_slots) {
            (true, _) => {
                // Stamp the project before anything is written: inodes
                // created beforehand keep the old project and escape
                // accounting.
                quota::assign_project(dir, resolved.project_id)
                    .map_err(|err| Status::internal(format!("assigning project: {err}")))?;
                quota::set_project_limit(
                    self.store.layout().root(),
                    resolved.project_id,
                    limit_bytes,
                )
                .map_err(|err| Status::internal(format!("setting quota: {err}")))?;
            }
            (false, true) => tracing::warn!(
                workspace,
                slot = %resolved.id,
                "filesystem has no project-quota enforcement and \
                 --allow-unquotaed-slots is set: this slot has NO capacity limit \
                 and can fill the node volume"
            ),
            (false, false) => {
                return Err(unquotaed_refusal(self.store.layout().root()));
            }
        }
        std::fs::set_permissions(
            dir,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )
        .map_err(|err| Status::internal(format!("setting slot permissions: {err}")))?;
        std::os::unix::fs::chown(dir, Some(SLOT_UID), Some(SLOT_GID))
            .map_err(|err| Status::internal(format!("chowning slot: {err}")))?;
        tracing::info!(
            workspace,
            slot = %resolved.id,
            project_id = resolved.project_id,
            limit_bytes,
            "allocated slot"
        );
        // Seed the venv from the node template before hydrating, so the
        // runner does not have to build ~920MB of it from scratch. Failure
        // is not fatal: `uv sync` will build one, just slowly.
        match crate::venv::seed_from_template(self.store.layout().root(), dir, python_runtime).await
        {
            Ok(seeded) => tracing::info!(workspace, seeded, "venv template"),
            Err(err) => tracing::warn!(%err, workspace, "could not seed venv template"),
        }
        // Only a freshly created slot is hydrated. Re-hydrating one that is
        // already populated would overwrite the tenant's newer local edits
        // with whatever was last synced — this is the path that makes
        // reopening a workspace with a warm slot instant.
        // Absent credentials leave both sources unreadable; the slot starts
        // empty rather than the mount failing, matching a workspace that
        // genuinely has no archive.
        let s3 = self.s3_for(namespace, workspace);
        let restored = self
            .hydrate_new_slot(workspace, &resolved.id, dir, archive, seed, s3.as_ref())
            .await?;
        if restored {
            // Restored files land as root; the runner is uid 1000.
            chown_tree(dir)
                .map_err(|err| Status::internal(format!("chowning hydrated slot: {err}")))?;
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl Node for KubimoNode {
    async fn node_get_info(
        &self,
        _request: Request<proto::NodeGetInfoRequest>,
    ) -> Result<Response<proto::NodeGetInfoResponse>, Status> {
        Ok(Response::new(proto::NodeGetInfoResponse {
            node_id: self.node_id.clone(),
            // 0 means "no limit": slots are directories, not attachments, which
            // is the entire point — Scaleway caps block volumes at 15 per node.
            max_volumes_per_node: 0,
            accessible_topology: None,
        }))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<proto::NodeGetCapabilitiesRequest>,
    ) -> Result<Response<proto::NodeGetCapabilitiesResponse>, Status> {
        // No STAGE_UNSTAGE_VOLUME: kubelet does not call NodeStageVolume for
        // inline ephemeral volumes, and there is no shared stage to set up.
        Ok(Response::new(proto::NodeGetCapabilitiesResponse {
            capabilities: vec![],
        }))
    }

    async fn node_publish_volume(
        &self,
        request: Request<proto::NodePublishVolumeRequest>,
    ) -> Result<Response<proto::NodePublishVolumeResponse>, Status> {
        let request = request.into_inner();
        // Refuse once shutdown has begun. Publishing here would hand a runner a slot on
        // a data volume that is about to be destroyed — manufacturing exactly the wedged
        // mount this shutdown is draining to avoid. kubelet retries with backoff, so the
        // pod simply waits until the replacement agent answers.
        if self.store.is_draining() {
            return Err(Status::unavailable("node agent is draining"));
        }
        if request.target_path.is_empty() {
            return Err(Status::invalid_argument("target_path is required"));
        }
        let workspace = request
            .volume_context
            .get(ATTR_WORKSPACE)
            .ok_or_else(|| {
                Status::invalid_argument(format!("volume attribute {ATTR_WORKSPACE:?} is required"))
            })?
            .clone();
        let limit_bytes = match request.volume_context.get(ATTR_LIMIT_BYTES) {
            None => self.default_limit_bytes,
            Some(raw) => raw.parse::<u64>().map_err(|_| {
                Status::invalid_argument(format!(
                    "volume attribute {ATTR_LIMIT_BYTES:?} must be a byte count, got {raw:?}"
                ))
            })?,
        };

        let archive =
            request
                .volume_context
                .get(ATTR_BUCKET)
                .map(|bucket| crate::hydrate::ArchiveLocation {
                    bucket: bucket.clone(),
                    key_prefix: request.volume_context.get(ATTR_KEY_PREFIX).cloned(),
                });
        // Only consulted when the workspace's own archive turns out to be
        // empty; this is `spec.restoreFrom` reaching the agent, since a pooled
        // workspace has no init Job to run a restore container in.
        let seed_secrets = match request.volume_context.get(ATTR_SEED_SECRETS) {
            None => Default::default(),
            // Only the controller writes this attribute, so an unparseable
            // value is a version skew bug to surface, not input to tolerate.
            Some(raw) => raw.parse().map_err(|_| {
                Status::invalid_argument(format!(
                    "volume attribute {ATTR_SEED_SECRETS:?} must be \"values\" or \
                     \"names-only\", got {raw:?}"
                ))
            })?,
        };
        let seed = request.volume_context.get(ATTR_SEED_BUCKET).map(|bucket| {
            crate::hydrate::SeedArchive {
                location: crate::hydrate::ArchiveLocation {
                    bucket: bucket.clone(),
                    key_prefix: request.volume_context.get(ATTR_SEED_KEY_PREFIX).cloned(),
                },
                secrets: seed_secrets,
            }
        });
        // Supplied by kubelet because the CSIDriver sets podInfoOnMount. Every
        // Workspace/WorkspaceDirectory lookup below is namespaced, and the
        // agent's own namespace is the wrong one.
        let namespace = request
            .volume_context
            .get(ATTR_POD_NAMESPACE)
            .cloned()
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "volume attribute {ATTR_POD_NAMESPACE:?} is required; the CSIDriver must set \
                     podInfoOnMount"
                ))
            })?;

        let python_runtime = request.volume_context.get(ATTR_PYTHON_RUNTIME);

        // Serialise everything that follows against any other publish of the
        // same workspace on this node — a cache job is deliberately co-located
        // with a live runner, so two first publishes race here. Held from slot
        // creation through the publish record and the bind mount, so a
        // concurrent first publish can neither allocate a second slot and orphan
        // this one's directory, nor bind-mount one slot while the publish record
        // names another. The reaper takes the same lock before reclaiming.
        let lock = self.store.lock_for(&namespace, &workspace);
        let _slot_guard = lock.lock().await;

        // Credentials for this workspace's archive, resolved by kubelet from the
        // volume's `nodePublishSecretRef` in the *pod's* namespace. Recorded
        // before anything touches S3, and kept afterwards because the matching
        // unpublish — where the final flush happens — carries no secrets.
        self.remember_credentials(&namespace, &workspace, &request.secrets);
        let (slot, slot_dir) = self
            .prepare_slot(
                &namespace,
                &workspace,
                limit_bytes,
                archive.as_ref(),
                seed.as_ref(),
                python_runtime.map(String::as_str),
            )
            .await?;
        self.publish_slot_status(&workspace, &namespace, &slot, limit_bytes, archive.as_ref())
            .await;
        if let Err(err) = self.store.record_publish(
            &request.volume_id,
            &crate::store::PublishedSlot {
                workspace: workspace.clone(),
                namespace: namespace.clone(),
                slot: slot.id,
                bucket: archive.as_ref().map(|a| a.bucket.clone()),
                key_prefix: archive.as_ref().and_then(|a| a.key_prefix.clone()),
            },
        ) {
            // Not fatal: the mount is what the pod needs. Losing the record
            // only means the flush on unpublish is skipped, and the slot
            // keeps the data on disk either way.
            tracing::warn!(%err, workspace, "could not record publish; flush on stop will be skipped");
        }
        if let Some(archive) = archive.as_ref() {
            self.start_watcher(
                &request.volume_id,
                &namespace,
                &workspace,
                &slot_dir,
                archive,
            )
            .await;
        }
        let target = Path::new(&request.target_path);
        let mounted = crate::mount::bind(&slot_dir, target, request.readonly)
            .map_err(|err| Status::internal(format!("publishing slot: {err}")))?;
        if mounted {
            tracing::info!(
                workspace,
                target = %target.display(),
                read_only = request.readonly,
                "published slot"
            );
        }
        Ok(Response::new(proto::NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<proto::NodeUnpublishVolumeRequest>,
    ) -> Result<Response<proto::NodeUnpublishVolumeResponse>, Status> {
        let request = request.into_inner();
        if request.target_path.is_empty() {
            return Err(Status::invalid_argument("target_path is required"));
        }
        let target = Path::new(&request.target_path);
        // Unknown volume: the agent restarted, or this workspace was never
        // published by us. Nothing to flush — but still fall through to unbind.
        // A read *error* (as opposed to a clean not-found) is not swallowed: it
        // means we cannot tell which workspace this volume belonged to, so the
        // final flush is skipped and the slot's newest work stays only on this
        // node.
        let published = match self.store.lookup_publish(&request.volume_id) {
            Ok(published) => published,
            Err(err) => {
                tracing::warn!(
                    %err,
                    "could not read the publish record; skipping the final flush, so any \
                     changes since the last sync stay only on this node"
                );
                None
            }
        };
        // Everything that decides and performs the final flush runs under the
        // workspace lock, taken *before* we forget our own record or check for
        // siblings — the same lock the publish path holds across slot resolution
        // and its record, and the reaper takes before reclaiming. Once it is
        // ours a concurrent publish of this workspace has either already recorded
        // itself — so the sibling check below sees it — or has not begun
        // resolving, and the reaper cannot interleave between this decision and
        // the flush. Without a publish record there is nothing to flush and
        // nothing to serialise, so that path stays lock-free.
        if let Some(published) = published.as_ref() {
            let lock = self
                .store
                .lock_for(&published.namespace, &published.workspace);
            let _guard = lock.lock().await;
            // Release our claim on the shared watcher. It is shared by every
            // volume published for this workspace on this node — a cache job
            // co-located with a live runner — so it only actually stops once the
            // last of them is gone.
            self.stop_watcher(
                &request.volume_id,
                &published.namespace,
                &published.workspace,
            );
            // Forget our own record BEFORE checking for siblings: two volumes of
            // one workspace unpublishing together must not each see the other's
            // record and both skip the final flush. The lock serialises them, so
            // exactly one now observes no sibling and flushes. Doing it whether
            // or not we go on to unmount also stops a stale record — the target
            // was already gone — from pinning the workspace as published for as
            // long as this data volume lives, which would permanently block the
            // reaper reclaiming its slot.
            self.store.forget_publish(&request.volume_id);
            // Flush only as the last volume out. While a sibling volume is still
            // published its shared watcher is still syncing this same tree; a
            // one-shot flush racing it would compute an independent diff and
            // could delete a tenant edit the watcher just wrote. The durability
            // boundary is unchanged: "the last time a runner stopped".
            match self.store.other_published_volumes(
                &published.namespace,
                &published.workspace,
                &request.volume_id,
            ) {
                Ok(true) => {
                    // A sibling volume still mounts this workspace, so keep the
                    // cached credentials: its own final unpublish carries no
                    // secrets and is the one that will need them to flush.
                    tracing::info!(
                        workspace = %published.workspace,
                        "another volume still mounts this workspace; deferring its flush"
                    );
                }
                Ok(false) => {
                    self.flush_published_slot(published).await;
                    // Nothing on this node mounts the workspace any more and its
                    // final flush has run, so drop the cached credentials — the
                    // one thing that still needed them is done. Held longer they
                    // would accumulate for every workspace the node ever served.
                    self.forget_credentials(&published.namespace, &published.workspace);
                }
                Err(err) => {
                    tracing::warn!(
                        %err,
                        workspace = %published.workspace,
                        "could not check for sibling volumes; flushing to be safe"
                    );
                    self.flush_published_slot(published).await;
                    self.forget_credentials(&published.namespace, &published.workspace);
                }
            }
        }
        let unmounted = crate::mount::unbind(target)
            .map_err(|err| Status::internal(format!("unpublishing slot: {err}")))?;
        // The slot itself deliberately survives: keeping it lets the next open
        // of this workspace skip hydration entirely. Reclaiming is the reaper's
        // job, not the unpublish path's.
        if unmounted {
            // kubelet creates the target directory, so it is ours to remove.
            let _ = std::fs::remove_dir(target);
        }
        tracing::info!(target = %target.display(), unmounted, "unpublished slot");
        Ok(Response::new(proto::NodeUnpublishVolumeResponse {}))
    }

    async fn node_stage_volume(
        &self,
        _request: Request<proto::NodeStageVolumeRequest>,
    ) -> Result<Response<proto::NodeStageVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "this driver does not advertise STAGE_UNSTAGE_VOLUME",
        ))
    }

    async fn node_unstage_volume(
        &self,
        _request: Request<proto::NodeUnstageVolumeRequest>,
    ) -> Result<Response<proto::NodeUnstageVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "this driver does not advertise STAGE_UNSTAGE_VOLUME",
        ))
    }

    async fn node_get_volume_stats(
        &self,
        _request: Request<proto::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<proto::NodeGetVolumeStatsResponse>, Status> {
        Err(Status::unimplemented(
            "this driver does not advertise GET_VOLUME_STATS",
        ))
    }

    async fn node_expand_volume(
        &self,
        _request: Request<proto::NodeExpandVolumeRequest>,
    ) -> Result<Response<proto::NodeExpandVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "slot capacity is changed via the project quota, not NodeExpandVolume",
        ))
    }
}

/// Build the gRPC server with both services registered.
///
/// Split out from [`serve`] so tests can drive it over a socket without
/// duplicating the registration, which is exactly where a proto or service-name
/// mismatch would hide.
fn router(node: KubimoNode) -> tonic::service::Routes {
    tonic::service::Routes::default()
        .add_service(IdentityServer::new(KubimoIdentity))
        .add_service(NodeServer::new(node))
}

/// Serve the plugin on a unix socket until `shutdown` resolves.
pub async fn serve(
    socket_path: &Path,
    node: KubimoNode,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error>> {
    // A stale socket from a previous run would make bind() fail with EADDRINUSE.
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    tracing::info!(socket = %socket_path.display(), node_id = node.node_id, "serving CSI plugin");
    tonic::transport::Server::builder()
        .add_routes(router(node))
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::UnixListenerStream::new(listener),
            shutdown,
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::identity_client::IdentityClient;
    use proto::node_client::NodeClient;
    use tokio::net::UnixStream;

    /// Whether a temp directory would land on a filesystem that *does* enforce
    /// project quotas.
    ///
    /// The refusal tests below assert the `(false, false)` branch, which only
    /// happens without enforcement. `tempfile` follows `TMPDIR`, so on a host
    /// whose temp filesystem is XFS mounted `prjquota` they would take the
    /// opposite branch and fail on a quota syscall this unprivileged test
    /// process cannot make — a failure about the host, not the code. CI and
    /// every dev machine seen so far are ext4/tmpfs, so this normally returns
    /// `false` and the tests run.
    fn quotas_would_be_enforced() -> bool {
        tempfile::tempdir()
            .is_ok_and(|dir| quota::project_quota_enforced(dir.path()).unwrap_or(false))
    }

    fn node() -> (tempfile::TempDir, KubimoNode) {
        let dir = tempfile::tempdir().unwrap();
        let store = SlotStore::new(crate::slot::SlotLayout::new(dir.path()));
        (
            dir,
            KubimoNode::new("test-node".into(), store, 1024, true, None),
        )
    }

    /// A workspace's watcher is shared by every volume published for it on this
    /// node — a cache job is deliberately co-located with a live runner — and
    /// must outlive all but the last of them. Two watchers on one slot would
    /// walk and upload the same tree twice with independent key sets, racing
    /// each other's directory writes.
    #[tokio::test]
    async fn a_shared_watcher_stops_only_when_its_last_volume_goes() {
        let (_dir, node) = node();
        let handle = tokio::spawn(std::future::pending::<()>());
        let key = KubimoNode::slot_key("platform", "bmow-shared");
        node.lock_watchers().insert(
            key.clone(),
            Watcher {
                handle,
                volumes: ["vol-runner".to_string(), "vol-cache".to_string()]
                    .into_iter()
                    .collect(),
            },
        );

        node.stop_watcher("vol-cache", "platform", "bmow-shared");
        let watchers = node.lock_watchers();
        let watcher = watchers
            .get(&key)
            .expect("the runner still needs the watcher");
        assert!(!watcher.handle.is_finished());
        assert_eq!(watcher.volumes.len(), 1);
        drop(watchers);

        node.stop_watcher("vol-runner", "platform", "bmow-shared");
        assert!(!node.lock_watchers().contains_key(&key));
    }

    /// Unpublishing a volume that was never watched must not disturb anything.
    #[tokio::test]
    async fn stopping_an_unknown_volume_is_a_no_op() {
        let (_dir, node) = node();
        node.stop_watcher("vol-unknown", "platform", "bmow-absent");
        assert!(node.lock_watchers().is_empty());
    }

    /// Unpublishing one of two volumes of a workspace must defer the flush to
    /// the last one out: it forgets only its own record and leaves the
    /// sibling's — and the shared watcher — in place, so that final flush still
    /// happens. The flush marker itself is not observable here (a flush needs
    /// S3 and a namespaced kube client, and the test node has neither), so this
    /// pins the record and watcher bookkeeping the deferral decision rests on.
    #[tokio::test]
    async fn unpublishing_one_of_two_volumes_defers_and_keeps_the_sibling() {
        let (dir, node) = node();
        let slot = node
            .store()
            .resolve_or_create("tenant-a", "workspace")
            .unwrap();
        for volume in ["csi-runner", "csi-cache"] {
            node.store()
                .record_publish(
                    volume,
                    &crate::store::PublishedSlot {
                        workspace: "workspace".into(),
                        namespace: "tenant-a".into(),
                        slot: slot.id.clone(),
                        bucket: None,
                        key_prefix: None,
                    },
                )
                .unwrap();
        }
        let key = KubimoNode::slot_key("tenant-a", "workspace");
        node.lock_watchers().insert(
            key.clone(),
            Watcher {
                handle: tokio::spawn(std::future::pending::<()>()),
                volumes: ["csi-runner".to_string(), "csi-cache".to_string()]
                    .into_iter()
                    .collect(),
            },
        );

        let target = dir.path().join("never-mounted");
        node.node_unpublish_volume(Request::new(proto::NodeUnpublishVolumeRequest {
            volume_id: "csi-cache".into(),
            target_path: target.display().to_string(),
        }))
        .await
        .unwrap();

        // Its own record is gone; the sibling's survives so the last unpublish
        // still knows to flush.
        assert!(node.store().lookup_publish("csi-cache").unwrap().is_none());
        assert!(node.store().lookup_publish("csi-runner").unwrap().is_some());
        let watchers = node.lock_watchers();
        let watcher = watchers.get(&key).expect("the runner still needs it");
        assert_eq!(watcher.volumes.len(), 1);
        assert!(!watcher.handle.is_finished());
    }

    /// Start the plugin on a temp socket and hand back a connected channel.
    async fn connected() -> (tempfile::TempDir, tonic::transport::Channel) {
        connected_with(true).await
    }

    async fn connected_with(
        allow_unquotaed_slots: bool,
    ) -> (tempfile::TempDir, tonic::transport::Channel) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("csi.sock");
        let store = SlotStore::new(crate::slot::SlotLayout::new(dir.path()));
        let node = KubimoNode::new("test-node".into(), store, 1024, allow_unquotaed_slots, None);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_routes(router(node))
                .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener))
                .await
        });
        // The URI is ignored for a unix socket, but tonic requires a valid one.
        let connect_to = socket.clone();
        let channel = tonic::transport::Endpoint::try_from("http://[::]:50051")
            .unwrap()
            .connect_with_connector(tower::service_fn(move |_| {
                let path = connect_to.clone();
                async move {
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                        UnixStream::connect(path).await?,
                    ))
                }
            }))
            .await
            .unwrap();
        (dir, channel)
    }

    /// kubelet reads the driver name from here and matches it against the
    /// `CSIDriver` object; a mismatch makes every mount fail with "driver not
    /// found".
    #[tokio::test]
    async fn identity_reports_the_registered_driver_name() {
        let (_dir, channel) = connected().await;
        let info = IdentityClient::new(channel)
            .get_plugin_info(proto::GetPluginInfoRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(info.name, DRIVER_NAME);
        assert!(!info.vendor_version.is_empty());
    }

    #[tokio::test]
    async fn node_reports_its_id_and_no_volume_limit() {
        let (_dir, channel) = connected().await;
        let info = NodeClient::new(channel)
            .node_get_info(proto::NodeGetInfoRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(info.node_id, "test-node");
        // Slots are directories, not attachments — the whole point.
        assert_eq!(info.max_volumes_per_node, 0);
    }

    /// Without the `workspace` attribute the driver cannot know whose slot to
    /// mount, and must refuse rather than invent one.
    #[tokio::test]
    async fn publish_requires_the_workspace_attribute() {
        let (_dir, channel) = connected().await;
        let status = NodeClient::new(channel)
            .node_publish_volume(proto::NodePublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: "/tmp/kubimo-test-target".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("workspace"));
    }

    #[tokio::test]
    async fn publish_rejects_a_non_numeric_limit() {
        let (_dir, channel) = connected().await;
        let status = NodeClient::new(channel)
            .node_publish_volume(proto::NodePublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: "/tmp/kubimo-test-target".into(),
                volume_context: [
                    (ATTR_WORKSPACE.to_string(), "bmow-abc".to_string()),
                    (ATTR_LIMIT_BYTES.to_string(), "not-a-number".to_string()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Unpublishing a path that was never mounted is a normal retry, not an
    /// error — kubelet repeats the call until it succeeds.
    #[tokio::test]
    async fn unpublish_is_idempotent_for_an_unmounted_target() {
        let (dir, channel) = connected().await;
        let target = dir.path().join("never-mounted");
        NodeClient::new(channel)
            .node_unpublish_volume(proto::NodeUnpublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: target.display().to_string(),
            })
            .await
            .expect("unpublish of an unmounted target should succeed");
    }

    /// The safety default: on a filesystem without project-quota enforcement
    /// (a temp dir is ext4/tmpfs here), publishing must refuse rather than
    /// silently hand out a slot with no capacity limit.
    #[tokio::test]
    async fn publish_refuses_unquotaed_slots_by_default() {
        if quotas_would_be_enforced() {
            return;
        }
        let (_dir, channel) = connected_with(false).await;
        let status = NodeClient::new(channel)
            .node_publish_volume(proto::NodePublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: "/tmp/kubimo-test-target-quota".into(),
                volume_context: [
                    (ATTR_WORKSPACE.to_string(), "bmow-abc".to_string()),
                    (ATTR_POD_NAMESPACE.to_string(), "platform".to_string()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(
            status.message().contains("prjquota"),
            "the error should say how to fix it, got: {}",
            status.message()
        );
    }

    /// Every Workspace lookup the agent makes is namespaced, and its own
    /// namespace is the wrong one — so refusing the mount is far better than
    /// proceeding and reading a live workspace as deleted, which skips its
    /// flush and makes its slot look reclaimable.
    #[tokio::test]
    async fn publish_requires_the_pod_namespace() {
        let (_dir, channel) = connected().await;
        let status = NodeClient::new(channel)
            .node_publish_volume(proto::NodePublishVolumeRequest {
                volume_id: "csi-abc".into(),
                target_path: "/tmp/kubimo-test-target-ns".into(),
                volume_context: [(ATTR_WORKSPACE.to_string(), "bmow-abc".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("podInfoOnMount"),
            "the error should name the CSIDriver setting that supplies it, got: {}",
            status.message()
        );
    }

    /// The traversal half of the symlink defence: a symlinked *directory* must
    /// not be descended into, or the agent would walk — and chown — a tree
    /// outside the slot entirely.
    #[tokio::test]
    async fn walkdir_does_not_descend_symlinked_directories() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"host file").unwrap();

        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();
        std::fs::write(slot.join("own-file"), b"mine").unwrap();
        std::os::unix::fs::symlink(&outside, slot.join("escape")).unwrap();

        let walked = walkdir(&slot).unwrap();
        // The symlink itself is visited (so it gets lchown'd) ...
        assert!(walked.iter().any(|p| p.ends_with("escape")));
        assert!(walked.iter().any(|p| p.ends_with("own-file")));
        // ... but nothing behind it is.
        assert!(
            !walked.iter().any(|p| p.ends_with("secret")),
            "walked outside the slot through a symlink: {walked:?}"
        );
    }

    /// A publish that fails provisioning must leave nothing behind: kubelet's
    /// retry would otherwise find a slot with `created: false`, skip quota and
    /// hydration entirely, and publish it with no capacity limit at all. A
    /// tmpfs temp dir has no project quota, so with `allow_unquotaed_slots` off
    /// provisioning refuses in its quota branch — the failure this exercises.
    #[tokio::test]
    async fn a_refused_publish_leaves_no_slot_behind() {
        if quotas_would_be_enforced() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = SlotStore::new(crate::slot::SlotLayout::new(dir.path()));
        let node = KubimoNode::new("test-node".into(), store, 1024, false, None);
        let err = node
            .prepare_slot("tenant-a", "workspace", 1024, None, None, None)
            .await
            .expect_err("must refuse unquotaed slots");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            node.store()
                .lookup("tenant-a", "workspace")
                .unwrap()
                .is_none()
        );
    }

    /// The `(false, false)` refusal must not remove a slot that already
    /// existed: unlike a freshly created one, it may hold tenant data, and a
    /// filesystem that lost `prjquota` across a remount is a reason to refuse
    /// the publish, not to destroy the slot.
    #[tokio::test]
    async fn a_refused_publish_on_an_existing_slot_keeps_it() {
        if quotas_would_be_enforced() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Record the slot directly through the store, bypassing full
        // provisioning (its chown step needs privileges this test does not
        // have) — `prepare_slot` only needs `resolved.created` to be `false`.
        let store = SlotStore::new(crate::slot::SlotLayout::new(dir.path()));
        store.resolve_or_create("tenant-a", "workspace").unwrap();

        let refusing = KubimoNode::new("test-node".into(), store, 1024, false, None);
        let err = refusing
            .prepare_slot("tenant-a", "workspace", 1024, None, None, None)
            .await
            .expect_err("must refuse an unquotaed publish even for an existing slot");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            refusing
                .store()
                .lookup("tenant-a", "workspace")
                .unwrap()
                .is_some(),
            "an existing slot must survive a refused publish"
        );
    }

    /// Advertising STAGE_UNSTAGE would make kubelet call NodeStageVolume, which
    /// this driver does not implement.
    #[tokio::test]
    async fn node_advertises_no_staging_capability() {
        let (_dir, channel) = connected().await;
        let caps = NodeClient::new(channel)
            .node_get_capabilities(proto::NodeGetCapabilitiesRequest {})
            .await
            .unwrap()
            .into_inner();
        assert!(caps.capabilities.is_empty());
    }
}
