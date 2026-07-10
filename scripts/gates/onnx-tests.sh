#!/usr/bin/env bash
set -euo pipefail

onnxruntime_version="1.24.2"
archive_name="onnxruntime-linux-x64-${onnxruntime_version}.tgz"
archive_sha256="43725474ba5663642e17684717946693850e2005efbd724ac72da278fead25e6"
runtime_sha256="ffc84d48e845cf0b562ba4ea5ca32aaafc0d4069019fef4f63095b307d0270ad"
download_url="https://github.com/microsoft/onnxruntime/releases/download/v${onnxruntime_version}/${archive_name}"

if command -v mise >/dev/null 2>&1; then
    cargo_cmd=(mise x rust@stable -- cargo)
else
    cargo_cmd=(cargo)
fi

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
    exec "${cargo_cmd[@]}" test --features onnx "$@"
fi

for command in curl flock mktemp sha256sum tar; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        echo "ONNX test gate requires '${command}'" >&2
        exit 2
    fi
done

repo_root="$(git rev-parse --show-toplevel)"
cache_root="${ORT_TEST_CACHE_DIR:-${repo_root}/target/onnxruntime-shared}"
runtime_dir="${cache_root}/onnxruntime-linux-x64-${onnxruntime_version}"
lib_dir="${runtime_dir}/lib"

mkdir -p "${cache_root}"
exec 9>"${cache_root}/.download.lock"
flock 9

if [[ ! -e "${lib_dir}/libonnxruntime.so" ]]; then
    if [[ -e "${runtime_dir}" ]]; then
        echo "ONNX Runtime cache is incomplete: ${runtime_dir}" >&2
        exit 1
    fi

    temp_dir="$(mktemp -d "${cache_root}/.download.XXXXXX")"
    cleanup() {
        rm -rf -- "${temp_dir}"
    }
    trap cleanup EXIT

    curl --fail --location --retry 3 --silent --show-error \
        --output "${temp_dir}/${archive_name}" "${download_url}"
    printf '%s  %s\n' "${archive_sha256}" "${temp_dir}/${archive_name}" | sha256sum --check --status
    tar -xzf "${temp_dir}/${archive_name}" -C "${temp_dir}"

    mv "${temp_dir}/onnxruntime-linux-x64-${onnxruntime_version}" "${runtime_dir}"
    trap - EXIT
    cleanup
fi

printf '%s  %s\n' "${runtime_sha256}" "${lib_dir}/libonnxruntime.so" \
    | sha256sum --check --status
flock --unlock 9
exec 9>&-

export ORT_LIB_PATH="${lib_dir}"
export ORT_PREFER_DYNAMIC_LINK=1
export LD_LIBRARY_PATH="${lib_dir}${LD_LIBRARY_PATH:+:${LD_LIBRARY_PATH}}"

exec "${cargo_cmd[@]}" test --features onnx "$@"
