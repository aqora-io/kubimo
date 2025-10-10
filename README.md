<p align="center">
  <img src="./.github/assets/kubimo-mascot.webp" width="150" alt="Kubimo Mascot" />
  <h1 align="center">kubimo</h1>
</p>

## Example

Make sure to have [minikube installed and running](https://minikube.sigs.k8s.io/docs/start)

To run the controller first setup your environment. Example .envrc:

```bash
export RUST_LOG=info
export KUBIMO__RUNNER_STATUS__RESOLUTION__METHOD="Ingress"
export KUBIMO__RUNNER_STATUS__RESOLUTION__HOST="http://$(minikube ip)"
export BUILDX_BAKE_FILE="docker-bake.hcl:docker-bake.dev.hcl"
export KUBIMO__MARIMO_IMAGE="ghcr.io/aqora-io/kubimo-marimo:dev"
```

Then setup minikube

```bash
sh scripts/setup-minikube-dev.sh # setup minikube
docker buildx bake marimo # build marimo image
minikube image load ghcr.io/aqora-io/kubimo-marimo:dev # load image into minikube
```

And finally run the controller

```bash
cargo run -p kubimo --example apply_crds # apply CRDs
cargo run # run controller
```

To create an example runner run

```bash
kubectl apply -f examples/basic.yaml
```

You should be able to access it soon with `$(minikube ip)/editor`
