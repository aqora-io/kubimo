use kubimo::{prelude::*, *};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 3 {
        return Err("Usage: viewer <workspace> <notebook>".into());
    }
    let workspace = &args[1];
    let notebook = &args[2];

    let client = Client::infer().await?;

    let bmows = client.api::<KubimoWorkspace>();
    let bmor = client.api::<KubimoRunner>();

    let workspace = bmows.get(workspace).await?;
    let runner = bmor
        .patch(&workspace.create_runner(KubimoRunnerSpec {
            command: KubimoRunnerCommand::Run,
            notebook: Some(notebook.to_string()),
            ..Default::default()
        })?)
        .await?;

    println!("Created runner: {}", runner.name()?);

    Ok(())
}
