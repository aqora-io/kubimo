use kubimo::{prelude::*, *};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 2 {
        return Err("Usage: viewer <workspace>".into());
    }
    let workspace = &args[1];

    let client = Client::infer().await?;

    let bmows = client.api::<KubimoWorkspace>();
    let bmor = client.api::<KubimoRunner>();

    let workspace = bmows.get(workspace).await?;
    let runner = bmor
        .patch(&workspace.create_runner(KubimoRunnerSpec {
            command: KubimoRunnerCommand::Run,
            ..Default::default()
        })?)
        .await?;

    println!("Created runner: {}", runner.name()?);

    Ok(())
}
