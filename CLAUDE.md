# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

kubimo is a Kubernetes operator for running [marimo](https://marimo.io) Python notebooks. It defines CRDs (group `kubimo.aqora.io/v1`) for notebook workspaces and runners, a controller that reconciles them into Pods/Services/Ingresses/PVCs/Jobs, and an indexer that syncs workspace files to S3.

## Commands

```bash
cargo build                          # builds kubimo-controller (default workspace member)
cargo run                            # run the controller against current kubeconfig
cargo test --workspace               # unit tests
cargo test -p kubimo <name>          # single test by name filter
cargo fmt --all && cargo clippy --workspace
```

Integration tests (`api/tests/integration.rs`) are all `#[ignore]`d because they need a live cluster; they apply CRDs and create/delete namespaces:

```bash
cargo test -p kubimo --test integration -- --ignored
```

### Local development against minikube

From the README — set up the environment (example `.envrc`):

```bash
export RUST_LOG=info
export KUBIMO__RUNNER_STATUS__RESOLUTION__METHOD="Ingress"
export KUBIMO__RUNNER_STATUS__RESOLUTION__HOST="http://$(minikube ip)"
export BUILDX_BAKE_FILE="docker-bake.hcl:docker-bake.dev.hcl"
export KUBIMO__MARIMO_IMAGE="ghcr.io/aqora-io/kubimo-marimo:dev"
```

```bash
sh scripts/setup-minikube-dev                              # minikube + ingress/CSI snapshot addons + MinIO
docker buildx bake marimo                                  # build marimo image (:dev tag via bake.dev file)
minikube image load ghcr.io/aqora-io/kubimo-marimo:dev
cargo run -p kubimo --example apply_crds                   # apply CRDs to the cluster
cargo run                                                  # run controller
kubectl apply -f examples/basic.yaml                       # create example Workspace + Runners
# then visit $(minikube ip)/editor
```

CRD schema changes require re-running `apply_crds` against the cluster.

### Docker images

`docker buildx bake` targets: `controller` (Dockerfile.controller) and `marimo` (Dockerfile.marimo). The marimo image builds marimo from the aqora-io fork — the git ref is pinned in the `MARIMO_GIT` variable in `docker-bake.hcl`; "update marimo" means bumping that pin. `docker/setup/` contains the files baked into the marimo image (start.sh, launch.py, marimo.toml, pyproject.toml).

## Architecture

### Crates

- `api/` (crate name **`kubimo`**) — CRD type definitions plus a typed client wrapper. Features: `client` + `runtime` (default), `ws`. Examples: `apply_crds`, `print_crds`.
- `controller/` (**`kubimo-controller`**, default member) — the operator binary.
- `indexer/` — binary that runs *inside* workspace pods (shipped in the marimo image): watches the workspace directory, parses notebooks with tree-sitter-python, uploads files/metadata to S3 via `object_store`, and writes `WorkspaceDirectory` CRs.
- `notebook_meta/` — serde types for marimo notebook metadata (shared by indexer / written to S3).
- `json-patch-macros/` — `patch!`/`path!`/`add!`/`put!`… macros for building JSON patches used in reconcilers.
- `k8s-crd-snapshot-storage/` — typed bindings for `snapshot.storage.k8s.io` VolumeSnapshot (used for workspace cloning).

### CRDs (`api/src/crd.rs`)

All defined with `#[derive(CustomResource, JsonSchema, ...)]` + `#[kube(...)]`, with CEL validation rules attached in the kube attribute:

- **Workspace** (`bmow`) — persistent notebook workspace. Reconciles into a PVC (optionally cloned from another workspace via VolumeSnapshot), an init-containers Job, and an indexer Pod + its ServiceAccount/Role/RoleBinding. Gets `Ready` condition once the PVC is bound.
- **Runner** (`bmor`) — one marimo process (`spec.command`: Edit / Run / Render) in a workspace (`spec.workspace`). Reconciles into Pod + Service + Ingress. Not reconciled until its Workspace is Ready; owned by the Workspace.
- **CacheJob** (`bmocj`) — a Job that pre-populates uv/marimo caches for a workspace.
- **WorkspaceDirectory** (`bmowd`) — directory listing + file metadata. Written by the **indexer**, not the controller (the controller only attaches owner references).

`all_crds()` returns the full set; factory helpers like `Workspace::new_runner()` set owner references.

### Controller (`controller/src/`)

`main.rs` spawns five controller loops with graceful shutdown: `workspace`, `workspace_directory`, `runner`, `runner_status`, `cache_job` (under `controllers/`).

- Reconcilers implement the `Reconciler` trait (`apply`/`cleanup`, `reconciler.rs`) and are wrapped via `ReconcilerExt` in a Tower stack: tracing → backoff → finalizer (`service.rs`). All cleanup goes through finalizers.
- Each resource's reconcile logic is split into `apply_*.rs` files (e.g. `controllers/runner/apply_pod.rs`); steps for a resource run concurrently with `join_all`.
- `runner_status` is poll-based, not watch-based: it hits the marimo HTTP API and writes `status.lastActive` / `status.marimoVersion`. Endpoint resolution is configured via `StatusCheckResolution`: `ServiceDns` (default, in-cluster) or `Ingress { host }` (used for local dev, where the controller runs outside the cluster).
- `workspace_affinity.rs`: all pods of a workspace get pod-affinity to the same node (the PVC is single-node).
- Config (`config.rs`) loads env vars with prefix `KUBIMO` and `__` separator, e.g. `KUBIMO__MARIMO_IMAGE`, `KUBIMO__RUNNER_STATUS__RESOLUTION__METHOD`.

### API client layer (`api/src/`)

`Client`/`ClientBuilder` wrap `kube::Client`; `Api<T>` wraps `kube::Api` with apply-style `patch()` (strips system/status fields), `patch_json`, and filtered list streams. Common extension traits (`ObjectMetaExt`, `ResourceOwnerRefExt`, …) live in the prelude. Typed quantities (`StorageQuantity`, `CpuQuantity`, `Requirement<T>` min/max) wrap `k8s_openapi::Quantity`.

### Helm chart (`charts/kubimo-controller/`)

Deploys the controller (Deployment + ClusterRole/Binding + ServiceAccount). `templates/crds.json` is **generated in CI** from the Rust types via `cargo run -p kubimo --example print_crds -- --helm-if .Values.crds.enabled` — never hand-edit chart CRDs; change `api/src/crd.rs` instead. Charts are published by the `cd.yaml` workflow with chart-releaser.

## Releases & conventions

- Version lives in the root `Cargo.toml` `[workspace.package]`. A release commit (`chore: release vX.Y.Z`) bumps it together with `charts/kubimo-controller/Chart.yaml` (`version` and `appVersion`).
- Commit messages follow Conventional Commits with optional scope: `feat(controller): ...`, `fix:`, `build:`, `chore:`, `test:`.
