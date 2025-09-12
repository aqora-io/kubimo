set -x
script_dir=$(dirname "$0")
# stop minikube if running
minikube stop
# start minikube with containerd
minikube start --container-runtime=containerd \
  --docker-opt containerd=/var/run/containerd/containerd.sock
# enable ingress addon
minikube addons enable ingress
# enable gvisor addon
minikube addons enable gvisor
# build images
docker buildx bake -f "$script_dir/../docker-bake.hcl"
# load images
minikube image load local/kubimo-marimo:dev
# use minikube context
kubectl config use-context minikube
# create kubimo namespace
kubectl create namespace kubimo
# set kubimo as default namespace
kubectl config set-context --current --namespace=kubimo
# create gitea
helm repo add gitea-charts https://dl.gitea.io/charts/
helm upgrade -n gitea --create-namespace --install gitea gitea-charts/gitea \
  --set gitea.admin.username=admin \
  --set gitea.admin.password=password
# create minio
helm repo add minio-operator https://operator.min.io
helm install \
  --namespace minio-operator \
  --create-namespace \
  operator minio-operator/operator
helm install \
  --namespace minio \
  --create-namespace \
  --set-json 'tenant.buckets=[{"name": "kubimo"}]' \
  minio minio-operator/tenant
kubectl create secret generic kubimo-s3-creds \
  --from-literal=AWS_ACCESS_KEY_ID=minio \
  --from-literal=AWS_SECRET_ACCESS_KEY=minio123 \
  --from-literal=AWS_ENDPOINT=https://minio.minio.svc.cluster.local \
  --from-literal=AWS_ALLOW_INVALID_CERTIFICATES=true
