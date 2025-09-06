set -x

script_dir=$(dirname "$0")
kubectl delete namespace kubimo
kubectl delete namespace gitea
rm -rf "$script_dir/../.keys"
