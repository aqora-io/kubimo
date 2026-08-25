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
        let mut owned = Workspace::new(TEST_WORKSPACE, spec_with_storage(Some("2Gi")));
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
        let mut changed = Workspace::new(TEST_WORKSPACE, spec_with_storage(Some("4Gi")));
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
                .and_then(|storage| storage.max)
                .map(|max| max.to_string()),
            Some("4Gi".to_string())
        );

        // Ownership really moved: the old name is now the one that conflicts.
        let mut reverted = Workspace::new(TEST_WORKSPACE, spec_with_storage(Some("2Gi")));
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
fn spec_with_storage(max: Option<&str>) -> WorkspaceSpec {
    WorkspaceSpec {
        storage: Some(kubimo::StorageRequirement {
            max: max.map(|q| q.parse().expect("a quantity")),
        }),
        ..Default::default()
    }
}

/// The CRD's CEL rules, exercised against a real API server. A rule is
/// otherwise only checked by asserting the generated CRD *contains* it, which
/// says nothing about what it means — only an API server evaluates the
/// expression.
///
/// A runner pinned to an exact size is a legitimate spec, and both rules'
/// messages promise "greater than or equal to". They compared strictly — a
/// defect first found on the since-removed workspace storage rule — so
/// `min == max` was refused at admission, which is precisely what a client
/// that sizes a runner's request and limit alike produces.
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

/// The Pool CRD's admission rules: Render and Conda are refused outright, and
/// command/pythonRuntime are pinned once created. These are the guards that
/// keep a pool from minting pods its claims could never safely serve.
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_pool_admission_rules() {
    with_namespace("test-pool-admission", |client, ns| async move {
        let pools = client.api_namespaced::<kubimo::Pool>(&ns);

        let mut editors = kubimo::Pool::new(
            "test-pool",
            kubimo::PoolSpec {
                replicas: 1,
                command: kubimo::RunnerCommand::Edit,
                ..Default::default()
            },
        );
        editors.metadata.namespace = Some(ns.clone());
        pools.patch(&editors).await.expect("an Edit pool applies");

        // Render cannot be pooled: a renderer's slot is bound read-only at
        // publish time, which an already-published anonymous slot cannot be.
        let mut renderers = kubimo::Pool::new(
            "test-pool-render",
            kubimo::PoolSpec {
                replicas: 1,
                command: kubimo::RunnerCommand::Render,
                ..Default::default()
            },
        );
        renderers.metadata.namespace = Some(ns.clone());
        assert!(
            pools.patch(&renderers).await.is_err(),
            "a Render pool must be refused"
        );

        // Conda cannot be pooled yet: its dependency sync cannot run before
        // marimo boots on a pre-booted pod.
        let mut conda = kubimo::Pool::new(
            "test-pool-conda",
            kubimo::PoolSpec {
                replicas: 1,
                command: kubimo::RunnerCommand::Edit,
                python_runtime: Some(kubimo::WorkspacePythonRuntime::Conda),
                ..Default::default()
            },
        );
        conda.metadata.namespace = Some(ns.clone());
        assert!(
            pools.patch(&conda).await.is_err(),
            "a Conda pool must be refused"
        );

        // Command is immutable: warm pods were booted with it baked in.
        let mut flipped = kubimo::Pool::new(
            "test-pool",
            kubimo::PoolSpec {
                replicas: 1,
                command: kubimo::RunnerCommand::Run,
                ..Default::default()
            },
        );
        flipped.metadata.namespace = Some(ns.clone());
        assert!(
            pools.patch(&flipped).await.is_err(),
            "changing a pool's command must be refused"
        );

        // Replicas is a sizing knob and stays mutable.
        let mut resized = kubimo::Pool::new(
            "test-pool",
            kubimo::PoolSpec {
                replicas: 3,
                command: kubimo::RunnerCommand::Edit,
                ..Default::default()
            },
        );
        resized.metadata.namespace = Some(ns.clone());
        pools
            .patch(&resized)
            .await
            .expect("resizing a pool must be accepted");
    })
    .await;
}

/// `spec.pool` is claim-once: a runner that cold-started must not grow a pool
/// later (the claim path would strand its pod), and a pooled runner must not
/// lose or change it (its status.claim would dangle).
#[tokio::test]
#[ignore = "requires a running Kubernetes cluster"]
async fn test_runner_pool_is_immutable() {
    with_namespace("test-runner-pool-immutable", |client, ns| async move {
        let runners = client.api_namespaced::<Runner>(&ns);

        let mut pooled = Runner::new(
            "test-pooled",
            RunnerSpec {
                workspace: TEST_WORKSPACE.to_string(),
                pool: Some("editors".to_string()),
                ..Default::default()
            },
        );
        pooled.metadata.namespace = Some(ns.clone());
        runners
            .patch(&pooled)
            .await
            .expect("a runner naming a pool applies");

        for (name, pool) in [
            ("test-pooled", None),                        // dropping it
            ("test-pooled", Some("viewers".to_string())), // changing it
        ] {
            let mut mutated = Runner::new(
                name,
                RunnerSpec {
                    workspace: TEST_WORKSPACE.to_string(),
                    pool,
                    ..Default::default()
                },
            );
            mutated.metadata.namespace = Some(ns.clone());
            assert!(
                runners.patch(&mutated).await.is_err(),
                "mutating spec.pool must be refused"
            );
        }

        // A pool-less runner gaining one later is refused too.
        let mut cold = Runner::new(
            "test-cold",
            RunnerSpec {
                workspace: TEST_WORKSPACE.to_string(),
                ..Default::default()
            },
        );
        cold.metadata.namespace = Some(ns.clone());
        runners
            .patch(&cold)
            .await
            .expect("a pool-less runner applies");
        let mut grown = Runner::new(
            "test-cold",
            RunnerSpec {
                workspace: TEST_WORKSPACE.to_string(),
                pool: Some("editors".to_string()),
                ..Default::default()
            },
        );
        grown.metadata.namespace = Some(ns.clone());
        assert!(
            runners.patch(&grown).await.is_err(),
            "a cold runner must not gain a pool"
        );
    })
    .await;
}
