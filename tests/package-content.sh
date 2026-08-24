#!/usr/bin/env bash

# Verify package scriptlets and the architecture-independent GUI archive.

set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_DIR
TEMP_DIR=""

fail() {
  echo "package content test: $*" >&2
  exit 1
}

cleanup() {
  [[ -z ${TEMP_DIR} ]] || rm -rf -- "${TEMP_DIR}"
}

main() {
  local archive
  local output
  local version

  TEMP_DIR="$(mktemp -d)"
  trap cleanup EXIT

  # shellcheck source=packaging/arch/milevox-omarchy/milevox-omarchy.install
  source "${REPO_DIR}/packaging/arch/milevox-omarchy/milevox-omarchy.install"
  declare -F pre_remove >/dev/null ||
    fail "Omarchy package scriptlet has no pre_remove hook"
  output="$(pre_remove)"
  [[ ${output} == *'milevox-omarchy uninstall'* ]] ||
    fail "pre_remove omits the GUI uninstall command"
  [[ ${output} == *'preserves'* ]] ||
    fail "pre_remove omits the user-data preservation notice"

  MILEVOX_DIST_DIR="${TEMP_DIR}/dist" \
    "${REPO_DIR}/scripts/package-omarchy-release.sh" >/dev/null
  version="$(awk -F '"' '/^version = / { print $2; exit }' \
    "${REPO_DIR}/Cargo.toml")"
  archive="${TEMP_DIR}/dist/milevox-omarchy-${version}.tar.gz"
  "${REPO_DIR}/scripts/check-release-archives.sh" \
    --omarchy-only "${archive}"

  echo "Package content tests passed."
}

main "$@"
