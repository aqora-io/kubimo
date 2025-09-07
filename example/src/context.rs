use std::net::IpAddr;

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
    pub async fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let client = kubimo::Client::infer().await?;
        let minikube_ip = get_minikube_ip().await.ok();
        Ok(Self {
            client,
            minikube_ip,
        })
    }
}
