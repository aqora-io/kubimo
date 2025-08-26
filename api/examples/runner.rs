use kubimo::StorageUnit::*;
use kubimo::{prelude::*, *};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::infer().await?;

    let bmows = client.api::<KubimoWorkspace>();
    let bmor = client.api::<KubimoRunner>();

    let workspace = bmows
        .patch(&KubimoWorkspace::create(KubimoWorkspaceSpec {
            min_storage: Some((2, Gi).into()),
            ..Default::default()
        }))
        .await?;

    println!("Created workspace: {}", workspace.name()?);

    let runner = bmor
        .patch(&workspace.create_runner(Default::default())?)
        .await?;

    println!("Created runner: {}", runner.name()?);

    Ok(())
}
