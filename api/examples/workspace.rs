use kubimo::StorageUnit::*;
use kubimo::{prelude::*, *};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() > 4 {
        return Err("Usage: workspace <repo> <ssh_key_path>".into());
    }
    let repo = args
        .get(1)
        .map(|repo| format!("ssh://git@gitea-ssh.gitea.svc.cluster.local:2222/{repo}"));
    let ssh_key = args
        .get(2)
        .map(|ssh_key_path| {
            std::fs::read_to_string(ssh_key_path)
                .map_err(|e| format!("Failed to read ssh key: {}", e))
        })
        .transpose()?;

    let client = Client::infer().await?;

    let bmows = client.api::<KubimoWorkspace>();

    let workspace = bmows
        .patch(&KubimoWorkspace::create(KubimoWorkspaceSpec {
            min_storage: Some((2, Gi).into()),
            repo,
            ssh_key,
            ..Default::default()
        }))
        .await?;

    println!("Created workspace: {}", workspace.name()?);

    Ok(())
}
