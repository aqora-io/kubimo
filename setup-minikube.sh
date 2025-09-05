set -x
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
docker buildx bake
# load images
minikube image load local/kubimo-marimo-init:dev
minikube image load local/kubimo-marimo-base:dev
# use minikube context
kubectl config use-context minikube
# create kubimo namespace
kubectl create namespace kubimo
# set kubimo as default namespace
kubectl config set-context --current --namespace=kubimo
# create gitea
helm repo add gitea-charts https://dl.gitea.io/charts/
helm upgrade -n gitea --create-namespace --install gitea gitea-charts/gitea \
  -f docker/gitea-values.yaml
# create an ssh key for gitea
temp_dir=$(mktemp -d)
ssh-keygen -t ed25519 -q -N "" -f $temp_dir/id_ed25519
kubectl create secret generic gitea-ssh-key \
  --from-file=id_ed25519=$temp_dir/id_ed25519 \
  --from-file=id_ed25519.pub=$temp_dir/id_ed25519.pub
rm -r $temp_dir
