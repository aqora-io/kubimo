<p align="center">
  <img src="./.github/assets/kubimo-mascot.webp" width="150" alt="Kubimo Mascot" />
  <h1 align="center">kubimo</h1>
</p>

## Example

Make sure to have [minikube installed and running](https://minikube.sigs.k8s.io/docs/start)

```bash
minikube addon enable ingress # enable ingress addon
eval $(minikube docker-env) # use minikube's docker daemon
docker buildx bake # build images
kubectl create namespace kubimo # create kubimo namespace
export KUBIMO_NAMESPACE=kubimo # set kubimo namespace for controller
export RUST_LOG=info # set log level
cargo run --example runner # provision with a workspace and runner
cargo run # run controller
```

You should be able to access it soon with `$(minikube ip)/<runner-name>`
