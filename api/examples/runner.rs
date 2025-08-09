use kubimo::StorageUnit::*;
use kubimo::{prelude::*, *};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .name("kubimo")
        .namespace("kubimo")
        .build()
        .await?;

    let bmows = client.api::<KubimoWorkspace>();
    let bmor = client.api::<KubimoRunner>();

    let workspace = KubimoWorkspace::create(KubimoWorkspaceSpec {
        min_storage: Some((2, Gi).into()),
        ..Default::default()
    });
    let workspace = bmows.patch(&workspace).await?;
    let runner = workspace.create_runner(Default::default())?;
    let mut runner = bmor.patch(&runner).await?;

    println!("Created runner: {}", serde_json::to_string_pretty(&runner)?);

    // runner workspace immutable
    runner.spec.workspace = "new-workspace".to_string();
    assert!(bmor.patch(&runner).await.is_err());

    Ok(())
}
