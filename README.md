<p align="center">
  <img src="./.github/assets/kubimo-mascot.webp" width="150" alt="Kubimo Mascot" />
  <h1 align="center">kubimo</h1>
</p>

## Example

Make sure to have [minikube installed and running](https://minikube.sigs.k8s.io/docs/start)

To run the controller run the following

```bash
sh scripts/setup-minikube-dev.sh # setup minikube
export RUST_LOG=info # set log level
cargo run -p kubimo --example apply_crds # apply CRDs
cargo run # run controller
```

To create an example runner run

```bash
kubectl apply -f examples/basic.yaml
```

You should be able to access it soon with `$(minikube ip)/editor`
