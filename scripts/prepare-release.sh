#!/usr/bin/env bash

# Update every file that stores the Milevox release version.

set -euo pipefail

TEMP_DIR=""

fail() {
  echo "prepare-release: $*" >&2
  exit 1
}

cleanup() {
  [[ -z "${TEMP_DIR}" ]] || rm -rf -- "${TEMP_DIR}"
}

usage() {
  echo "usage: $0 <version>" >&2
}

require_command() {
  local command_name="$1"

  command -v "${command_name}" >/dev/null ||
    fail "${command_name} is required"
}

render_cargo_manifest() {
  local source="$1"
  local destination="$2"
  local version="$3"

  awk -v version="${version}" '
    $0 == "[package]" {
      in_package = 1
    }
    in_package && /^\[/ && $0 != "[package]" {
      in_package = 0
    }
    in_package && /^version = / {
      print "version = \"" version "\""
      updated++
      next
    }
    { print }
    END {
      if (updated != 1) exit 1
    }
  ' "${source}" >"${destination}"
}

render_cargo_lock() {
  local source="$1"
  local destination="$2"
  local version="$3"

  awk -v version="${version}" '
    $0 == "[[package]]" {
      in_package = 1
      is_milevox = 0
    }
    in_package && $0 == "name = \"milevox\"" {
      is_milevox = 1
    }
    is_milevox && /^version = / {
      print "version = \"" version "\""
      updated++
      is_milevox = 0
      next
    }
    { print }
    END {
      if (updated != 1) exit 1
    }
  ' "${source}" >"${destination}"
}

main() {
  local version="${1:-}"
  local version_pattern
  local repo_dir
  local staged_manifest
  local staged_lock
  local staged_plugin_manifest
  local manifest_version
  local lock_version
  local plugin_version

  if [[ $# -ne 1 ]]; then
    usage
    exit 1
  fi
  version_pattern='^[0-9]+\.[0-9]+\.[0-9]+'
  version_pattern+='(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
  [[ "${version}" =~ ${version_pattern} ]] ||
    fail "invalid version: ${version}"

  require_command awk
  require_command install
  require_command jq
  require_command mktemp

  repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
  TEMP_DIR="$(mktemp -d)"
  staged_manifest="${TEMP_DIR}/Cargo.toml"
  staged_lock="${TEMP_DIR}/Cargo.lock"
  staged_plugin_manifest="${TEMP_DIR}/manifest.json"
  trap cleanup EXIT

  render_cargo_manifest \
    "${repo_dir}/Cargo.toml" "${staged_manifest}" "${version}"
  render_cargo_lock \
    "${repo_dir}/Cargo.lock" "${staged_lock}" "${version}"
  jq --arg version "${version}" '.version = $version' \
    "${repo_dir}/guis/omarchy/manifest.json" >"${staged_plugin_manifest}"

  manifest_version="$(
    awk -F '"' '/^version = / { print $2; exit }' "${staged_manifest}"
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
    ' "${staged_lock}"
  )"
  plugin_version="$(jq -r '.version // empty' "${staged_plugin_manifest}")"
  [[ "${manifest_version}" == "${version}" ]] ||
    fail "could not update Cargo.toml"
  [[ "${lock_version}" == "${version}" ]] ||
    fail "could not update Cargo.lock"
  [[ "${plugin_version}" == "${version}" ]] ||
    fail "could not update the Omarchy manifest"

  install -m 0644 "${staged_manifest}" "${repo_dir}/Cargo.toml"
  install -m 0644 "${staged_lock}" "${repo_dir}/Cargo.lock"
  install -m 0644 "${staged_plugin_manifest}" \
    "${repo_dir}/guis/omarchy/manifest.json"

  echo "Prepared release ${version}."
}

main "$@"
