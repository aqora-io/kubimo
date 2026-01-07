use kubimo::{
    Client, Result, all_crds,
    k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
};

#[cfg(feature = "client")]
#[tokio::main]
pub async fn main() -> Result<()> {
    let client = Client::infer().await?;
    let crds = client.api_global::<CustomResourceDefinition>();
    for crd in all_crds() {
        println!("Applying {}", crd.metadata.name.as_ref().unwrap());
        crds.patch(&crd).await?;
    }
    Ok(())
}
