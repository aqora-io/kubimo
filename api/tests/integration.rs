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
