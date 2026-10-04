//! Condition types kubimo resources report: a `Runner` as it starts up, an
//! `ImportJob` once it finishes.
//!
//! These strings are a public contract, not an implementation detail. Consumers
//! match on them byte-exactly and treat a *missing* condition as unsatisfied, so
//! renaming one does not surface as an error anywhere — it silently pins every
//! runner at the phase before it, forever. They live here rather than in the
//! controller so that both the writer and its readers compile against the same
//! constant.

/// The workspace's storage is attached and usable: the runner's slot on the
/// node data volume is mounted (or, for a claimed warm pod, the claim is
/// acked).
///
/// Named for the retired `Dedicated` mechanism (a bound PVC) and deliberately
/// kept under that name: consumers match the string byte-exactly, and renaming
/// it would silently pin every runner at the phase before it.
pub const PVC_BOUND: &str = "PvcBound";
/// The runner's `Workspace` reports `Ready`.
pub const WORKSPACE_READY: &str = "WorkspaceReady";
/// The runner's pod has been assigned to a node.
pub const POD_SCHEDULED: &str = "PodScheduled";
/// The runner's pod is passing its readiness probe.
pub const POD_READY: &str = "PodReady";

/// Every startup condition, in the order they are fulfilled.
pub const STARTUP_CONDITIONS: [&str; 4] = [PVC_BOUND, WORKSPACE_READY, POD_SCHEDULED, POD_READY];

/// An `ImportJob`'s import succeeded: every file is in the workspace.
/// Terminal.
pub const IMPORT_COMPLETE: &str = "Complete";
/// An `ImportJob`'s import failed — fetching a file, converting it, writing
/// it, or running out of time. Terminal. Its reason is one of the `IMPORT_*`
/// reasons below, or else Kubernetes' (`DeadlineExceeded`, `Evicted`); its
/// message says what went wrong.
pub const IMPORT_FAILED: &str = "Failed";

/// [`IMPORT_FAILED`] reason: a file could not be fetched from S3. The message
/// names its bucket and key.
pub const IMPORT_FETCH_FAILED: &str = "FetchFailed";
/// [`IMPORT_FAILED`] reason: a file could not be converted or written into the
/// workspace. The message names its output path; nothing was written.
pub const IMPORT_WRITE_FAILED: &str = "ImportFailed";
/// [`IMPORT_FAILED`] reason: the apiserver refused the import's Job, so it
/// never ran. The message is the apiserver's.
pub const IMPORT_JOB_REJECTED: &str = "JobRejected";
/// [`IMPORT_FAILED`] reason: a `secretName` names a Secret that still did not
/// exist a minute after the ImportJob was created, so the import never ran.
pub const IMPORT_SECRET_NOT_FOUND: &str = "SecretNotFound";
