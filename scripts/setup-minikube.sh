#!/usr/bin/env sh

minikube start --container-runtime=containerd \
  --network=minikube \
  --docker-opt containerd=/var/run/containerd/containerd.sock \
  --addons=gvisor,metrics-server,dashboard,csi-hostpath-driver \
  ${MINIKUBE_SUBNET:+--subnet=$MINIKUBE_SUBNET}

# Runners are routed by Envoy Gateway now (below). A cluster created before
# that still has the ingress addon on; it is not needed any more.
minikube addons disable ingress || true

# Shamelessly copy-pasted from:
# https://minikube.sigs.k8s.io/docs/tutorials/volume_snapshots_and_csi/
minikube addons disable storage-provisioner
minikube addons disable default-storageclass
kubectl patch storageclass csi-hostpath-sc -p '{"metadata": {"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}'

# Envoy Gateway, with the Gateway API CRDs. Runner HTTPRoutes attach to the
# `kubimo` Gateway below; see "Routing: Ingress vs Gateway API" in the README.
helm upgrade --install eg oci://docker.io/envoyproxy/gateway-helm \
  --version v1.10.0 \
  --namespace envoy-gateway-system --create-namespace \
  --wait
kubectl wait --timeout=5m -n envoy-gateway-system deployment/envoy-gateway --for=condition=Available

# Inline rather than in files of their own: the platform's setup script fetches
# only this one file.
#
# The proxy is exposed on nodePort 31439, which the platform's traefik proxies
# /runner and /marimo-assets to. The traffic policies match ingress-nginx's
# 3600 s proxy timeouts: Envoy's 5-minute stream-idle default would cut a
# kernel websocket that is silent while a cell runs.
kubectl apply -f - <<'EOF'
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: EnvoyProxy
metadata:
  name: kubimo
  namespace: envoy-gateway-system
spec:
  provider:
    type: Kubernetes
    kubernetes:
      envoyService:
        type: NodePort
        patch:
          type: StrategicMerge
          value:
            spec:
              ports:
                - port: 80
                  nodePort: 31439
---
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: kubimo
spec:
  controllerName: gateway.envoyproxy.io/gatewayclass-controller
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: kubimo
  namespace: envoy-gateway-system
spec:
  gatewayClassName: kubimo
  infrastructure:
    parametersRef:
      group: gateway.envoyproxy.io
      kind: EnvoyProxy
      name: kubimo
  listeners:
    - name: http
      protocol: HTTP
      port: 80
      allowedRoutes:
        namespaces:
          from: All
---
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: ClientTrafficPolicy
metadata:
  name: kubimo
  namespace: envoy-gateway-system
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: kubimo
  timeout:
    http:
      idleTimeout: 3600s
      streamIdleTimeout: 3600s
---
apiVersion: gateway.envoyproxy.io/v1alpha1
kind: BackendTrafficPolicy
metadata:
  name: kubimo
  namespace: envoy-gateway-system
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: kubimo
  timeout:
    http:
      connectionIdleTimeout: 3600s
      streamIdleTimeout: 3600s
EOF
kubectl wait --timeout=5m -n envoy-gateway-system gateway/kubimo --for=condition=Programmed
