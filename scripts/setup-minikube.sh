#!/usr/bin/env sh

minikube start --container-runtime=containerd \
  --network=minikube \
  --docker-opt containerd=/var/run/containerd/containerd.sock \
  --addons=ingress,gvisor,metrics-server,dashboard
