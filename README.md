<p align="center">
  <img src="./.github/assets/kubimo-mascot.webp" width="150" alt="Kubimo Mascot" />
  <h1 align="center">kubimo</h1>
</p>

## Example

Make sure to have [minikube installed and running](https://minikube.sigs.k8s.io/docs/start). You also need to add the minikube registry to the list of insecure registries. You can find the minikube IP with `minikube ip` and add it to `/etc/docker/daemon.json` like so:

```json
{
  "insecure-registries": ["<minikube ip>:5000"]
}
```

To run the example run the following

```bash
sh setup-minikube.sh # setup minikube
export RUST_LOG=info # set log level
cargo run --example runner # provision with a workspace and runner
cargo run # run controller
```

You should be able to access it soon with `$(minikube ip)/<runner-name>`
