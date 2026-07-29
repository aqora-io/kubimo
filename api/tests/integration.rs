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
