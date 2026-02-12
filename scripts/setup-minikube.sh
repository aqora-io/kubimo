#!/usr/bin/env sh

# start minikube with containerd
minikube start --container-runtime=containerd \
  --docker-opt containerd=/var/run/containerd/containerd.sock \
  --addons=ingress,gvisor
