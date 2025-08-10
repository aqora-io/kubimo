# stop minikube if running
minikube stop
# start minikube with containerd
minikube start --container-runtime=containerd \
  --docker-opt containerd=/var/run/containerd/containerd.sock
# enable ingress addon
minikube addons enable ingress
# enable gvisor addon
minikube addons enable gvisor
# enable registry addon
minikube addons enable registry
# push images to minikube registry
REGISTRY="$(minikube ip):5000" docker buildx bake --push
# use minikube context
kubectl config use-context minikube
# create kubimo namespace
kubectl create namespace kubimo
# set kubimo as default namespace
kubectl config set-context --current --namespace=kubimo
