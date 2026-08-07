use std::future::Future;

use kubimo::{
    Client, Runner, RunnerSpec, Workspace, WorkspaceSpec, all_crds,
    k8s_openapi::{
        api::core::v1::Namespace,
        apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
    },
};

const TEST_WORKSPACE: &str = "test-workspace";
const TEST_RUNNER: &str = "test-runner";

async fn setup_client() -> Client {
    Client::infer()
        .await
        .expect("Failed to infer client from kubeconfig")
}

async fn apply_crds(client: &Client) {
    let crds = client.api_global::<CustomResourceDefinition>();
    for crd in all_crds() {
        let name = crd.metadata.name.as_deref().unwrap_or("unknown");
        crds.patch(&crd)
            .await
            .unwrap_or_else(|e| panic!("Failed to apply CRD {name}: {e}"));
    }
}

async fn with_namespace<F, Fut>(ns_name: &str, f: F)
where
    F: FnOnce(Client, String) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let client = setup_client().await;
    apply_crds(&client).await;

    let ns_api = client.api_global::<Namespace>();
    // A namespace from a previous run may still be Terminating, and creating
    // objects in one that is fails. The names are fixed per test, so running
    // the suite twice in a row — or retrying a flake in CI — hits this reliably
    // enough to look like a real failure. Wait it out rather than racing it.
    for attempt in 0..60 {
        match ns_api.get_opt(ns_name).await {
            Ok(Some(existing))
                if existing
                    .status
                    .as_ref()
                    .and_then(|status| status.phase.as_deref())
                    == Some("Terminating") =>
            {
                assert!(
                    attempt < 59,
                    "namespace {ns_name} was still Terminating after 60s"
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            _ => break,
        }
    }

    let mut ns = Namespace::default();
    ns.metadata.name = Some(ns_name.to_string());
    ns_api.patch(&ns).await.expect("Failed to create namespace");

    let ns_name_owned = ns_name.to_string();
    let result = tokio::spawn(async move { f(client, ns_name_owned).await }).await;

    ns_api
        .delete(ns_name)
        .await
        .expect("Failed to delete namespace");

    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_infer_client() {
    setup_client().await;
}

#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_apply_crds() {
    let client = setup_client().await;
    apply_crds(&client).await;
}

#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_create_workspace() {
    with_namespace("test-create-workspace", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);
        let workspace = Workspace::new(TEST_WORKSPACE, WorkspaceSpec::default());
        let created = workspaces
            .patch(&workspace)
            .await
            .expect("Failed to create workspace");
        assert_eq!(created.metadata.name.as_deref(), Some(TEST_WORKSPACE));
    })
    .await;
}

#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_create_runner_for_workspace() {
    with_namespace("test-create-runner", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);
        let runners = client.api_namespaced::<Runner>(&ns);

        let workspace = Workspace::new(TEST_WORKSPACE, WorkspaceSpec::default());
        let created_workspace = workspaces
            .patch(&workspace)
            .await
            .expect("Failed to create workspace");
        assert_eq!(
            created_workspace.metadata.name.as_deref(),
            Some(TEST_WORKSPACE)
        );

        let runner = created_workspace
            .new_runner(TEST_RUNNER, RunnerSpec::default())
            .expect("Failed to build runner");
        println!("{}", serde_json::to_string_pretty(&runner).unwrap());
        let created_runner = runners
            .patch(&runner)
            .await
            .expect("Failed to create runner");

        assert_eq!(created_runner.metadata.name.as_deref(), Some(TEST_RUNNER));
        assert_eq!(created_runner.spec.workspace, TEST_WORKSPACE);
    })
    .await;
}

/// The agent's status fields must survive the indexer's status writes.
///
/// `status.slot` and `status.archive` are written by the node agent;
/// `status.storage` is written by the indexer, repeatedly, for the life of a
/// runner. Under server-side apply a manager owns exactly the fields its last
/// apply contained, so a manager that omits a field *relinquishes* it — and the
/// indexer's patches never mention slot or archive.
///
/// Writing all three under one identity therefore had the API server delete the
/// slot status seconds after the agent wrote it, with no error on either side.
/// This asserts the property that prevents it: the two managers own disjoint
/// fields and neither can revoke the other's.
///
/// `skip_serializing_if` on those structs is a different guard — it stops a
/// manager writing an explicit `null` over someone else's field. It cannot stop
/// a manager silently dropping its own.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_agent_status_survives_indexer_writes() {
    with_namespace("test-status-managers", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);
        workspaces
            .patch(&Workspace::new(TEST_WORKSPACE, WorkspaceSpec::default()))
            .await
            .expect("Failed to create workspace");

        // The agent records where the slot lives, under its own identity.
        let agent = Client::builder()
            .name("kubimo-agent")
            .namespace(&ns)
            .build()
            .await
            .expect("Failed to build an agent client");
        let mut from_agent = Workspace::new(TEST_WORKSPACE, WorkspaceSpec::default());
        from_agent.status = Some(kubimo::WorkspaceStatus {
            slot: Some(kubimo::WorkspaceSlotStatus {
                node: Some("node-1".to_string()),
                id: Some("slot-abc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        agent
            .api_namespaced::<Workspace>(&ns)
            .patch_status(&from_agent)
            .await
            .expect("agent failed to write slot status");

        // The indexer then reports usage, repeatedly, never mentioning the slot.
        let indexer = Client::builder()
            .name("kubimo-indexer")
            .namespace(&ns)
            .build()
            .await
            .expect("Failed to build an indexer client");
        for used in ["100", "200", "300"] {
            let mut from_indexer = Workspace::new(TEST_WORKSPACE, WorkspaceSpec::default());
            from_indexer.status = Some(kubimo::WorkspaceStatus {
                storage: Some(kubimo::WorkspaceStorageStatus {
                    used: Some(used.parse().expect("a quantity")),
                    ..Default::default()
                }),
                ..Default::default()
            });
            indexer
                .api_namespaced::<Workspace>(&ns)
                .patch_status(&from_indexer)
                .await
                .expect("indexer failed to write storage status");
        }

        let found = workspaces
            .get(TEST_WORKSPACE)
            .await
            .expect("Failed to read workspace back");
        let status = found.status.expect("status is set");
        let slot = status
            .slot
            .expect("the agent's slot status was deleted by the indexer's writes");
        assert_eq!(slot.id.as_deref(), Some("slot-abc"));
        assert_eq!(slot.node.as_deref(), Some("node-1"));
        // ...and the indexer's own field still updates.
        assert_eq!(
            status
                .storage
                .and_then(|storage| storage.used)
                .map(|used| used.to_string()),
            Some("300".to_string())
        );
    })
    .await;
}

/// A rename of the field manager is only survivable because an apply can be
/// forced.
///
/// Ownership under server-side apply is per field and per manager name, so
/// renaming a client's manager leaves every field of every object it ever wrote
/// belonging to the old name. The first write to each object then conflicts —
/// permanently, since nothing else is going to relinquish those fields. Forcing
/// is how the new name takes them over, and it must stay a per-call choice:
/// `patch` conflicting is what
/// [`test_agent_status_survives_indexer_writes`] depends on.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_a_forced_apply_takes_ownership_from_another_manager() {
    with_namespace("test-force-apply", |_client, ns| async move {
        let old = Client::builder()
            .name("kubimo")
            .namespace(&ns)
            .build()
            .await
            .expect("Failed to build a client for the old manager");
        let mut owned = Workspace::new(TEST_WORKSPACE, spec_with_storage(Some("2Gi"), None));
        owned.metadata.namespace = Some(ns.clone());
        old.api_namespaced::<Workspace>(&ns)
            .patch(&owned)
            .await
            .expect("the old manager creates the workspace and owns its fields");

        // The same client under its new name, changing a field the old name
        // owns. This is every existing object, on the first write after a
        // rename.
        let renamed = Client::builder()
            .name("aqora-platform")
            .namespace(&ns)
            .build()
            .await
            .expect("Failed to build a client for the renamed manager");
        let mut changed = Workspace::new(TEST_WORKSPACE, spec_with_storage(Some("4Gi"), None));
        changed.metadata.namespace = Some(ns.clone());
        assert!(
            renamed
                .api_namespaced::<Workspace>(&ns)
                .patch(&changed)
                .await
                .is_err(),
            "an unforced apply must not take a field another manager owns"
        );

        let applied = renamed
            .api_namespaced::<Workspace>(&ns)
            .patch_force(&changed)
            .await
            .expect("a forced apply takes the field over");
        assert_eq!(
            applied
                .spec
                .storage
                .and_then(|storage| storage.min)
                .map(|min| min.to_string()),
            Some("4Gi".to_string())
        );

        // Ownership really moved: the old name is now the one that conflicts.
        let mut reverted = Workspace::new(TEST_WORKSPACE, spec_with_storage(Some("2Gi"), None));
        reverted.metadata.namespace = Some(ns.clone());
        assert!(
            old.api_namespaced::<Workspace>(&ns)
                .patch(&reverted)
                .await
                .is_err(),
            "the forced apply must leave the field owned by the manager that forced it"
        );
    })
    .await;
}

/// Build a spec with a storage requirement, leaving everything else default.
fn spec_with_storage(min: Option<&str>, max: Option<&str>) -> WorkspaceSpec {
    WorkspaceSpec {
        storage: Some(kubimo::StorageRequirement {
            min: min.map(|q| q.parse().expect("a quantity")),
            max: max.map(|q| q.parse().expect("a quantity")),
            auto: None,
        }),
        ..Default::default()
    }
}

/// The CRD's CEL rules, exercised against a real API server.
///
/// These are otherwise only checked by asserting the generated CRD *contains*
/// the rule, which says nothing about what it means. That gap is not
/// hypothetical: `workspace_max_storage_greater_than_min` compared with a
/// strict `isGreaterThan` while promising "greater than or equal to", so
/// `min == max` — a Pooled workspace, whose slot quota is fixed — was refused at
/// admission. The rule was present, its message was right, and every unit test
/// passed. Only an API server evaluates the expression.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_storage_validation_accepts_equal_min_and_max() {
    with_namespace("test-cel-storage", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);

        // A workspace that does not grow: its floor is its ceiling.
        let mut equal = Workspace::new("test-equal", spec_with_storage(Some("64Gi"), Some("64Gi")));
        equal.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&equal)
            .await
            .expect("min == max must be accepted; the rule promises >=, not >");

        // Room to grow is still fine.
        let mut wider = Workspace::new("test-wider", spec_with_storage(Some("2Gi"), Some("64Gi")));
        wider.metadata.namespace = Some(ns.clone());
        workspaces.patch(&wider).await.expect("min < max");

        // A ceiling below the floor is still nonsense and must be refused.
        let mut inverted = Workspace::new(
            "test-inverted",
            spec_with_storage(Some("64Gi"), Some("2Gi")),
        );
        inverted.metadata.namespace = Some(ns.clone());
        assert!(
            workspaces.patch(&inverted).await.is_err(),
            "max < min must be rejected"
        );
    })
    .await;
}

/// A runner pinned to an exact size is a legitimate spec, and both rules'
/// messages promise "greater than or equal to".
///
/// They compared strictly — the defect already found and fixed for workspace
/// storage — so `min == max` was refused at admission, which is precisely what
/// a client that sizes a runner's request and limit alike produces.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_runner_validation_accepts_equal_min_and_max() {
    with_namespace("test-cel-runner-bounds", |client, ns| async move {
        let runners = client.api_namespaced::<Runner>(&ns);

        let mut pinned = Runner::new(
            "test-pinned",
            RunnerSpec {
                workspace: TEST_WORKSPACE.to_string(),
                cpu: Some(kubimo::Requirement {
                    min: Some("500m".parse().expect("a quantity")),
                    max: Some("500m".parse().expect("a quantity")),
                }),
                memory: Some(kubimo::Requirement {
                    min: Some("2Gi".parse().expect("a quantity")),
                    max: Some("2Gi".parse().expect("a quantity")),
                }),
                ..Default::default()
            },
        );
        pinned.metadata.namespace = Some(ns.clone());
        runners
            .patch(&pinned)
            .await
            .expect("min == max must be accepted; the rules promise >=, not >");

        // A ceiling below the floor is still nonsense and must be refused.
        let mut inverted = Runner::new(
            "test-inverted-cpu",
            RunnerSpec {
                workspace: TEST_WORKSPACE.to_string(),
                cpu: Some(kubimo::Requirement {
                    min: Some("2".parse().expect("a quantity")),
                    max: Some("500m".parse().expect("a quantity")),
                }),
                ..Default::default()
            },
        );
        inverted.metadata.namespace = Some(ns.clone());
        assert!(
            runners.patch(&inverted).await.is_err(),
            "cpu max < min must be rejected"
        );

        let mut inverted_memory = Runner::new(
            "test-inverted-memory",
            RunnerSpec {
                workspace: TEST_WORKSPACE.to_string(),
                memory: Some(kubimo::Requirement {
                    min: Some("4Gi".parse().expect("a quantity")),
                    max: Some("2Gi".parse().expect("a quantity")),
                }),
                ..Default::default()
            },
        );
        inverted_memory.metadata.namespace = Some(ns.clone());
        assert!(
            runners.patch(&inverted_memory).await.is_err(),
            "memory max < min must be rejected"
        );
    })
    .await;
}

/// `cloneWorkspaceName` is implemented only for `Dedicated` — its readers are
/// the workspace reconciler's `apply_pvc` and `apply_job`, neither of which
/// runs under `Pooled`. Before the rule existed the field was accepted and then
/// ignored, and the workspace came up with an empty slot: the CR applied, the
/// runner went Ready, and the user's notebook was simply absent.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_clone_is_refused_for_pooled_workspaces() {
    with_namespace("test-cel-clone", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);

        let mut pooled = Workspace::new(
            "test-pooled-clone",
            WorkspaceSpec {
                mode: Some(kubimo::WorkspaceMode::Pooled),
                clone_workspace_name: Some("bmow-source".to_string()),
                ..Default::default()
            },
        );
        pooled.metadata.namespace = Some(ns.clone());
        assert!(
            workspaces.patch(&pooled).await.is_err(),
            "cloneWorkspaceName under Pooled is read by nothing and must be refused, \
             not silently ignored"
        );

        // Dedicated really does clone the PVC, so clone stays legal there. A
        // fresh Dedicated is now refused at create, so the only Dedicated that
        // can exist is one the controller already materialized: grandfather one
        // via status, then clone it.
        let mut grandfathered = Workspace::new("test-dedicated-clone", WorkspaceSpec::default());
        grandfathered.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&grandfathered)
            .await
            .expect("creating a workspace without a mode");

        let mut materialized = Workspace::new("test-dedicated-clone", WorkspaceSpec::default());
        materialized.status = Some(kubimo::WorkspaceStatus {
            mode: Some(kubimo::WorkspaceMode::Dedicated),
            ..Default::default()
        });
        workspaces
            .patch_status(&materialized)
            .await
            .expect("materializing status.mode: Dedicated");

        let mut cloned = Workspace::new(
            "test-dedicated-clone",
            WorkspaceSpec {
                clone_workspace_name: Some("bmow-source".to_string()),
                ..Default::default()
            },
        );
        cloned.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&cloned)
            .await
            .expect("Dedicated clones the source PVC and must still be allowed");
    })
    .await;
}

/// The same refusal, for the workspace that never says Pooled in its spec.
///
/// On a cluster whose default mode is Pooled — which is what the rollout makes
/// of production — a client-created workspace has no `spec.mode`, and
/// `status.mode` is the only record that it is Pooled. A rule keyed on the spec
/// admitted `cloneWorkspaceName` on exactly those, and the controller then went
/// looking for a source PVC that pooled mode never creates: the workspace came
/// up empty, Ready, and silent.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_clone_is_refused_for_status_materialized_pooled_workspaces() {
    with_namespace("test-cel-status-clone", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);

        // No mode in the spec: the client left it to the operator default.
        let mut workspace = Workspace::new("test-status-clone", WorkspaceSpec::default());
        workspace.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&workspace)
            .await
            .expect("creating a workspace without a mode");

        // What the controller writes once it resolves that default.
        let mut materialized = Workspace::new("test-status-clone", WorkspaceSpec::default());
        materialized.status = Some(kubimo::WorkspaceStatus {
            mode: Some(kubimo::WorkspaceMode::Pooled),
            ..Default::default()
        });
        workspaces
            .patch_status(&materialized)
            .await
            .expect("materializing status.mode");

        let mut cloned = Workspace::new(
            "test-status-clone",
            WorkspaceSpec {
                clone_workspace_name: Some("bmow-source".to_string()),
                ..Default::default()
            },
        );
        cloned.metadata.namespace = Some(ns.clone());
        assert!(
            workspaces.patch(&cloned).await.is_err(),
            "a materialized Pooled status must count as Pooled: there is no PVC to clone"
        );

        // The rule must still leave everything else about the workspace
        // writable, or the controller could not reconcile it at all.
        let mut edited = Workspace::new("test-status-clone", spec_with_storage(Some("2Gi"), None));
        edited.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&edited)
            .await
            .expect("a spec change that does not clone must still be accepted");
    })
    .await;
}

/// Restoring from an archive and cloning a PVC are two ways to seed the same
/// workspace, and doing both would race.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_restore_from_and_clone_are_mutually_exclusive() {
    with_namespace("test-cel-exclusive", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);
        let mut both = Workspace::new(
            "test-both",
            WorkspaceSpec {
                clone_workspace_name: Some("bmow-source".to_string()),
                restore_from: Some(kubimo::WorkspaceRestoreFrom {
                    bucket: "some-bucket".to_string(),
                    key_prefix: Some("workspace/other/".to_string()),
                    pod: None,
                }),
                ..Default::default()
            },
        );
        both.metadata.namespace = Some(ns.clone());
        assert!(
            workspaces.patch(&both).await.is_err(),
            "restoreFrom and cloneWorkspaceName must not both be set"
        );
    })
    .await;
}

/// A workspace's mode is permanent once chosen. Pooled has no PVC, so going
/// back to Dedicated would leave it with nowhere to put its files — the CEL
/// rule refuses the transition rather than letting a reconcile discover it.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_pooled_cannot_be_downgraded_to_dedicated() {
    with_namespace("test-cel-downgrade", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);

        let mut pooled = Workspace::new(
            "test-downgrade",
            WorkspaceSpec {
                mode: Some(kubimo::WorkspaceMode::Pooled),
                ..Default::default()
            },
        );
        pooled.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&pooled)
            .await
            .expect("creating a Pooled workspace");

        let mut downgraded = Workspace::new(
            "test-downgrade",
            WorkspaceSpec {
                mode: Some(kubimo::WorkspaceMode::Dedicated),
                ..Default::default()
            },
        );
        downgraded.metadata.namespace = Some(ns.clone());
        assert!(
            workspaces.patch(&downgraded).await.is_err(),
            "Pooled -> Dedicated must be refused"
        );
    })
    .await;
}

/// The same guard, for the workspace that never says Pooled in its spec.
///
/// On a cluster whose default mode is Pooled, a client-created workspace has no
/// `spec.mode` at all — the controller materializes `status.mode: Pooled` on the
/// first reconcile, and that is the only record of the choice. A rule keyed on
/// `oldSelf.spec.mode` sees nothing there and admits `spec.mode: Dedicated`,
/// leaving an object that contradicts itself until something drops the status
/// and the controller provisions a PVC for files that live in a pooled slot.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_status_materialized_pooled_cannot_be_downgraded() {
    with_namespace("test-cel-status-downgrade", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);

        // No mode in the spec: the client left it to the operator default.
        let mut workspace = Workspace::new("test-status-mode", WorkspaceSpec::default());
        workspace.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&workspace)
            .await
            .expect("creating a workspace without a mode");

        // What the controller writes once it resolves the default.
        let mut materialized = Workspace::new("test-status-mode", WorkspaceSpec::default());
        materialized.status = Some(kubimo::WorkspaceStatus {
            mode: Some(kubimo::WorkspaceMode::Pooled),
            ..Default::default()
        });
        workspaces
            .patch_status(&materialized)
            .await
            .expect("materializing status.mode");

        let mut downgraded = Workspace::new(
            "test-status-mode",
            WorkspaceSpec {
                mode: Some(kubimo::WorkspaceMode::Dedicated),
                ..Default::default()
            },
        );
        downgraded.metadata.namespace = Some(ns.clone());
        assert!(
            workspaces.patch(&downgraded).await.is_err(),
            "a materialized Pooled status must count as Pooled"
        );

        // ...and the rule must not have gone the other way and demanded a Pooled
        // spec, which no status-only-Pooled workspace can satisfy: an unrelated
        // spec change has to keep working.
        let mut edited = Workspace::new("test-status-mode", spec_with_storage(Some("2Gi"), None));
        edited.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&edited)
            .await
            .expect("a spec change unrelated to the mode must still be accepted");
    })
    .await;
}

/// `Dedicated` is deprecated: no new workspace may be created in it, but the ones
/// that already exist must keep working. The grandfather signal is `status.mode`
/// — forge-proof, since status is ignored on create — so a fresh
/// `spec.mode: Dedicated` is refused while a materialized Dedicated stays fully
/// writable.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_new_dedicated_is_refused_but_existing_is_grandfathered() {
    with_namespace("test-cel-no-new-dedicated", |client, ns| async move {
        let workspaces = client.api_namespaced::<Workspace>(&ns);

        // A fresh, explicit Dedicated is refused at create.
        let mut fresh = Workspace::new(
            "test-fresh-dedicated",
            WorkspaceSpec {
                mode: Some(kubimo::WorkspaceMode::Dedicated),
                ..Default::default()
            },
        );
        fresh.metadata.namespace = Some(ns.clone());
        assert!(
            workspaces.patch(&fresh).await.is_err(),
            "a newly-submitted Dedicated workspace must be refused"
        );

        // An existing Dedicated — one the controller already materialized — is
        // grandfathered: create without a mode, materialize status.mode:
        // Dedicated, and a later spec change (even one that re-states
        // spec.mode: Dedicated, as the platform does) must still be accepted.
        let mut existing = Workspace::new("test-existing-dedicated", WorkspaceSpec::default());
        existing.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&existing)
            .await
            .expect("creating a workspace without a mode");

        let mut materialized = Workspace::new("test-existing-dedicated", WorkspaceSpec::default());
        materialized.status = Some(kubimo::WorkspaceStatus {
            mode: Some(kubimo::WorkspaceMode::Dedicated),
            ..Default::default()
        });
        workspaces
            .patch_status(&materialized)
            .await
            .expect("materializing status.mode: Dedicated");

        let mut updated = Workspace::new(
            "test-existing-dedicated",
            WorkspaceSpec {
                mode: Some(kubimo::WorkspaceMode::Dedicated),
                ..spec_with_storage(Some("2Gi"), None)
            },
        );
        updated.metadata.namespace = Some(ns.clone());
        workspaces
            .patch(&updated)
            .await
            .expect("an existing Dedicated workspace must stay writable");
    })
    .await;
}
