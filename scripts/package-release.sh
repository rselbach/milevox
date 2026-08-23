#!/usr/bin/env bash

# Build a native Milevox release archive for the current Linux architecture.

set -euo pipefail

TEMP_DIR=""

fail() {
  echo "package-release: $*" >&2
  exit 1
}

cleanup() {
  [[ -z "${TEMP_DIR}" ]] || rm -rf -- "${TEMP_DIR}"
}

require_command() {
  local command_name="$1"

  command -v "${command_name}" >/dev/null ||
    fail "${command_name} is required"
}

normalize_architecture() {
  case "$1" in
    aarch64 | arm64)
      echo "aarch64"
      ;;
    x86_64 | amd64)
      echo "x86_64"
      ;;
    *)
      fail "unsupported architecture: $1"
      ;;
  esac
}

main() {
  local repo_dir
  local dist_dir
  local stage_parent
  local stage_dir
  local version
  local architecture
  local archive_name
  local archive_path
  local checksum_path

  require_command cargo
  require_command awk
  require_command install
  require_command sha256sum
  require_command tar

  repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
  dist_dir="${repo_dir}/dist"
  version="$(
    awk -F '"' '/^version = / { print $2; exit }' \
      "${repo_dir}/Cargo.toml"
  )"
  [[ -n "${version}" ]] || fail "could not read the Cargo package version"

  architecture="$(normalize_architecture "$(uname -m)")"
  archive_name="milevox-linux-${architecture}.tar.gz"
  archive_path="${dist_dir}/${archive_name}"
  checksum_path="${archive_path}.sha256"
  TEMP_DIR="$(mktemp -d)"
  stage_parent="${TEMP_DIR}"
  trap cleanup EXIT
  stage_dir="${stage_parent}/milevox-linux-${architecture}"

  cargo build --release --locked --manifest-path "${repo_dir}/Cargo.toml"

  mkdir -p -- "${stage_dir}/bin" "${stage_dir}/packaging/systemd" \
    "${stage_dir}/scripts"
  install -m 0755 "${repo_dir}/target/release/milevox" \
    "${stage_dir}/bin/milevox"
  install -m 0755 "${repo_dir}/install.sh" "${stage_dir}/install.sh"
  install -m 0755 "${repo_dir}/uninstall.sh" "${stage_dir}/uninstall.sh"
  install -m 0755 "${repo_dir}/scripts/download-model.sh" \
    "${stage_dir}/scripts/download-model.sh"
  install -m 0644 "${repo_dir}/packaging/systemd/milevox.service" \
    "${repo_dir}/packaging/systemd/environment" \
    "${stage_dir}/packaging/systemd/"
  install -m 0644 "${repo_dir}/README.md" "${repo_dir}/LICENSE" \
    "${repo_dir}/CHANGELOG.md" "${stage_dir}/"

  mkdir -p -- "${dist_dir}"
  rm -f -- "${archive_path}" "${checksum_path}"
  tar --create --gzip --file "${archive_path}" \
    --owner 0 --group 0 --numeric-owner --mtime '@0' \
    --directory "${stage_parent}" "$(basename -- "${stage_dir}")"
  (
    cd -- "${dist_dir}"
    sha256sum -- "${archive_name}" >"${archive_name}.sha256"
  )

  echo "Release archive: ${archive_path}"
  echo "Checksum: ${checksum_path}"
}

main "$@"
