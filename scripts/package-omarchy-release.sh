#!/usr/bin/env bash

# Build the architecture-independent Milevox integration archive for Omarchy.

set -euo pipefail
# shellcheck source=scripts/lib-release.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/lib-release.sh"

TEMP_DIR=""

fail() {
  echo "package-omarchy-release: $*" >&2
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
  local stage_dir
  local version
  local manifest_version
  local archive_name
  local archive_path

  require_command awk
  require_command install
  require_command jq
  require_command sha256sum
  require_command tar

  repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
  dist_dir="${repo_dir}/dist"
  version="$(project_version "${repo_dir}")" || fail "invalid project version"
  manifest_version="$(jq -r '.version // empty' \
    "${repo_dir}/guis/omarchy/manifest.json")"
  [[ "${manifest_version}" == "${version}" ]] ||
    fail "Omarchy manifest version ${manifest_version} does not match ${version}"

  archive_name="milevox-omarchy-${version}.tar.gz"
  archive_path="${dist_dir}/${archive_name}"
  mkdir -p -- "${dist_dir}"
  TEMP_DIR="$(mktemp -d "${dist_dir}/.milevox-omarchy-package.XXXXXX")"
  trap cleanup EXIT
  stage_dir="${TEMP_DIR}/milevox-omarchy-${version}"

  mkdir -p -- "${stage_dir}"
  install -m 0755 \
    "${repo_dir}/guis/omarchy/install.sh" \
    "${repo_dir}/guis/omarchy/uninstall.sh" \
    "${repo_dir}/guis/omarchy/milevox-omarchy" \
    "${repo_dir}/guis/omarchy/bindings-common.sh" \
    "${stage_dir}/"
  install -m 0644 \
    "${repo_dir}/guis/omarchy/manifest.json" \
    "${repo_dir}/guis/omarchy/Panel.qml" \
    "${repo_dir}/guis/omarchy/MilevoxOverlay.qml" \
    "${repo_dir}/guis/omarchy/MilevoxStatus.qml" \
    "${repo_dir}/guis/omarchy/README.md" \
    "${repo_dir}/LICENSE" \
    "${stage_dir}/"

  tar --create --gzip --file "${TEMP_DIR}/${archive_name}" \
    --sort=name --owner 0 --group 0 --numeric-owner --mtime '@0' \
    --directory "${TEMP_DIR}" "$(basename -- "${stage_dir}")"
  ( cd -- "${TEMP_DIR}"; sha256sum -- "${archive_name}" >"${archive_name}.sha256" )
  mv -f -- "${TEMP_DIR}/${archive_name}" "${archive_path}"
  mv -f -- "${TEMP_DIR}/${archive_name}.sha256" "${archive_path}.sha256"

  echo "Omarchy release archive: ${archive_path}"
  echo "Checksum: ${archive_path}.sha256"
}

main "$@"
