use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kubimo::{Client, Result, all_crds};

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::infer().await?;
    let crds = client.api_global::<CustomResourceDefinition>();
    for crd in all_crds() {
        crds.patch(&crd).await?;
    }
    Ok(())
}
