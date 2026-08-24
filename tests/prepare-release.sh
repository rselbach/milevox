#!/usr/bin/env bash

# Verify that release preparation updates every version source.

set -euo pipefail

readonly TEST_VERSION="9.8.7"

TEMP_DIR=""

fail() {
  echo "prepare release test: $*" >&2
  exit 1
}

cleanup() {
  [[ -z "${TEMP_DIR}" ]] || rm -rf -- "${TEMP_DIR}"
}

main() {
  local repo_dir
  local test_repo
  local manifest_version
  local lock_version
  local plugin_version

  repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
  TEMP_DIR="$(mktemp -d)"
  test_repo="${TEMP_DIR}/milevox"

  mkdir -p -- "${test_repo}/guis/omarchy" "${test_repo}/scripts"
  install -m 0644 "${repo_dir}/Cargo.toml" "${test_repo}/Cargo.toml"
  install -m 0644 "${repo_dir}/Cargo.lock" "${test_repo}/Cargo.lock"
  install -m 0644 "${repo_dir}/guis/omarchy/manifest.json" \
    "${test_repo}/guis/omarchy/manifest.json"
  install -m 0755 "${repo_dir}/scripts/prepare-release.sh" \
    "${test_repo}/scripts/prepare-release.sh"
  install -m 0644 "${repo_dir}/scripts/lib-release.sh" \
    "${test_repo}/scripts/lib-release.sh"

  if "${test_repo}/scripts/prepare-release.sh" v1.2.3 \
    >/dev/null 2>&1; then
    fail "accepted a version with a v prefix"
  fi
  for invalid in 01.2.3 1.02.3 1.2.03 1.2.3-rc1 1.2.3+build; do
    if "${test_repo}/scripts/prepare-release.sh" "${invalid}" >/dev/null 2>&1; then
      fail "accepted invalid version ${invalid}"
    fi
  done
  "${test_repo}/scripts/prepare-release.sh" "${TEST_VERSION}" \
    >/dev/null

  manifest_version="$(
    awk -F '"' '/^version = / { print $2; exit }' \
      "${test_repo}/Cargo.toml"
  )"
  lock_version="$(
    awk '
      $0 == "[[package]]" { is_milevox = 0 }
      $0 == "name = \"milevox\"" { is_milevox = 1; next }
      is_milevox && /^version = / {
        gsub(/^version = "|"$/, "")
        print
        exit
      }
    ' "${test_repo}/Cargo.lock"
  )"
  plugin_version="$(
    jq -r '.version // empty' \
      "${test_repo}/guis/omarchy/manifest.json"
  )"

  [[ "${manifest_version}" == "${TEST_VERSION}" ]] ||
    fail "Cargo.toml has version ${manifest_version}"
  [[ "${lock_version}" == "${TEST_VERSION}" ]] ||
    fail "Cargo.lock has version ${lock_version}"
  [[ "${plugin_version}" == "${TEST_VERSION}" ]] ||
    fail "the Omarchy manifest has version ${plugin_version}"
}

trap cleanup EXIT
main "$@"
