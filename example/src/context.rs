use std::net::IpAddr;

use clap::Args;

#[derive(Args, Debug, Default, Clone)]
pub struct GlobalArgs {
    #[arg(global = true, long, short = 'n')]
    namespace: Option<String>,
}

pub struct Context {
    pub client: kubimo::Client,
    pub minikube_ip: Option<IpAddr>,
}

async fn get_minikube_ip() -> Result<IpAddr, Box<dyn std::error::Error>> {
    let minikube_ip_out = tokio::process::Command::new("minikube")
        .arg("ip")
        .output()
        .await?;
    if !minikube_ip_out.status.success() {
        return Err(String::from_utf8_lossy(&minikube_ip_out.stderr)
            .as_ref()
            .into());
    }
    Ok(String::from_utf8(minikube_ip_out.stdout)?
        .trim()
        .parse::<IpAddr>()?)
}

impl Context {
    pub async fn load(global: GlobalArgs) -> Result<Self, Box<dyn std::error::Error>> {
        let mut client_builder = kubimo::Client::builder();
        if let Some(namespace) = global.namespace {
            client_builder.namespace(namespace);
        }
        let client = client_builder.build().await?;
        let minikube_ip = get_minikube_ip().await.ok();
        Ok(Self {
            client,
            minikube_ip,
        })
    }
}
