#!/usr/bin/env bash
set -xeuo pipefail

if ! command -v kopium >/dev/null 2>&1; then
  echo "Please install kopium (https://github.com/kube-rs/kopium?tab=readme-ov-file#installation)"
  exit 1
fi
if ! command -v curl >/dev/null 2>&1; then
  echo "Please install curl"
  exit 1
fi

script_dir="$(dirname "$0")"
version="v8.5.0"
base_url="https://github.com/kubernetes-csi/external-snapshotter/raw/refs/tags/${version}/client/config/crd"
group="snapshot.storage.k8s.io"
crds=(
  "volumesnapshotclasses"
  "volumesnapshotcontents"
  "volumesnapshots"
)
curl_flags=(
  --silent
  --fail
  --show-error
  --location
  --retry 3
)
kopium_flags=(
  -A
  --derive=Default
  --derive=PartialEq
  --smart-derive-elision
)

pushd "${script_dir}"

# Download CRDs in parallel
for crd_name in "${crds[@]}"; do
  crd_file="${group}_${crd_name}.yaml"
  crd_url="${base_url}/${crd_file}"
  curl "${curl_flags[@]}" --output "${crd_file}" "${crd_url}" &
done

wait # Until all files have downloaded successfully

# Generate Rust files
for crd_name in "${crds[@]}"; do
  crd_file="${group}_${crd_name}.yaml"
  kopium "${kopium_flags[@]}" -f "${crd_file}" >"src/${crd_name}.rs"
done

popd
