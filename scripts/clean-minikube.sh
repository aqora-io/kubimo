set -x

script_dir=$(dirname "$0")
kubectl delete namespace kubimo
kubectl delete namespace gitea
kubectl delete namespace minio
kubectl delete namespace minio-controller
rm -rf "$script_dir/../.keys"
