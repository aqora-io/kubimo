#!/usr/bin/env sh

minikube start --container-runtime=containerd \
  --network=minikube \
  --docker-opt containerd=/var/run/containerd/containerd.sock \
  --addons=ingress,gvisor,metrics-server,dashboard,volumesnapshots,csi-hostpath-driver

# Shamelessly copy-pasted from:
# https://minikube.sigs.k8s.io/docs/tutorials/volume_snapshots_and_csi/
minikube addons disable storage-provisioner
minikube addons disable default-storageclass
kubectl patch storageclass csi-hostpath-sc -p '{"metadata": {"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}'
kubectl patch volumesnapshotclass csi-hostpath-snapclass --type=merge -p '{"metadata":{"annotations":{"snapshot.storage.kubernetes.io/is-default-class":"true"}}}'
