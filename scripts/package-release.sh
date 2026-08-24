#!/usr/bin/env bash

# Build a native Milevox release archive for the current Linux architecture.

set -euo pipefail

# shellcheck source=scripts/lib-release.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/lib-release.sh"

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

main() {
  local repo_dir
  local dist_dir
  local stage_parent
  local stage_dir
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
  project_version "${repo_dir}" >/dev/null || fail "invalid project version"

  architecture="$(normalize_architecture "$(uname -m)")"
  archive_name="milevox-linux-${architecture}.tar.gz"
  archive_path="${dist_dir}/${archive_name}"
  checksum_path="${archive_path}.sha256"
  mkdir -p -- "${dist_dir}"
  TEMP_DIR="$(mktemp -d "${dist_dir}/.milevox-package.XXXXXX")"
  stage_parent="${TEMP_DIR}"
  trap cleanup EXIT
  stage_dir="${stage_parent}/milevox-linux-${architecture}"

  cargo build --release --locked --manifest-path "${repo_dir}/Cargo.toml"

  mkdir -p -- "${stage_dir}/bin" "${stage_dir}/packaging/systemd" \
    "${stage_dir}/scripts" "${stage_dir}/docs"
  install -m 0755 "${repo_dir}/target/release/milevox" \
    "${stage_dir}/bin/milevox"
  install -m 0755 "${repo_dir}/install.sh" "${stage_dir}/install.sh"
  install -m 0755 "${repo_dir}/uninstall.sh" "${stage_dir}/uninstall.sh"
  install -m 0755 "${repo_dir}/scripts/download-model.sh" \
    "${stage_dir}/scripts/download-model.sh"
  install -m 0755 "${repo_dir}/scripts/setup-user.sh" \
    "${stage_dir}/scripts/setup-user.sh"
  install -m 0755 "${repo_dir}/scripts/teardown-user.sh" \
    "${stage_dir}/scripts/teardown-user.sh"
  install -m 0644 "${repo_dir}/scripts/lib-release.sh" \
    "${stage_dir}/scripts/lib-release.sh"
  install -m 0644 "${repo_dir}/packaging/systemd/milevox.service" \
    "${repo_dir}/packaging/systemd/environment" \
    "${stage_dir}/packaging/systemd/"
  install -m 0644 "${repo_dir}/README.md" "${repo_dir}/LICENSE" \
    "${stage_dir}/"
  install -m 0644 "${repo_dir}/docs/configuration.md" \
    "${repo_dir}/docs/diagnostics.md" "${repo_dir}/docs/privacy.md" \
    "${stage_dir}/docs/"

  tar --create --gzip --file "${TEMP_DIR}/${archive_name}" \
    --sort=name --owner 0 --group 0 --numeric-owner --mtime '@0' \
    --directory "${stage_parent}" "$(basename -- "${stage_dir}")"
  ( cd -- "${TEMP_DIR}"; sha256sum -- "${archive_name}" >"${archive_name}.sha256" )
  mv -f -- "${TEMP_DIR}/${archive_name}" "${archive_path}"
  mv -f -- "${TEMP_DIR}/${archive_name}.sha256" "${checksum_path}"

  echo "Release archive: ${archive_path}"
  echo "Checksum: ${checksum_path}"
}

main "$@"
