#!/usr/bin/env bash

# Verify release archive checksums, members, documentation, and symbol stripping.

set -euo pipefail

TEMP_DIR=""

fail() {
  echo "check-release-archives: $*" >&2
  exit 1
}

cleanup() {
  [[ -z ${TEMP_DIR} ]] || rm -rf -- "${TEMP_DIR}"
}

require_member() {
  local members="$1"
  local member="$2"

  grep -Fx -- "${member}" "${members}" >/dev/null ||
    fail "archive member is missing: ${member}"
}

verify_checksum() {
  local archive="$1"
  local directory
  local checksum

  directory="$(dirname -- "${archive}")"
  checksum="$(basename -- "${archive}").sha256"
  [[ -f "${directory}/${checksum}" ]] ||
    fail "checksum is missing for ${archive}"
  (cd -- "${directory}" && sha256sum --check "${checksum}")
}

verify_readme_links() {
  local root="$1"
  local readme="$2"
  local link
  local path

  while IFS= read -r link; do
    link="${link#']('}"
    link="${link%')'}"
    case "${link}" in
      https://*) ;;
      *)
        path="${link%%#*}"
        [[ -e "${root}/${path}" ]] ||
          fail "packaged README link is not available: ${link}"
        ;;
    esac
  done < <(grep -oE ']\([^)]+\)' "${readme}" || true)
}

verify_core_archive() {
  local archive="$1"
  local name
  local architecture
  local root
  local members
  local binary
  local sections
  local extracted
  local readme
  local downloader
  local member
  local expected

  name="$(basename -- "${archive}")"
  [[ ${name} =~ ^milevox-linux-(x86_64|aarch64)\.tar\.gz$ ]] ||
    fail "invalid core archive name: ${name}"
  architecture="${BASH_REMATCH[1]}"
  root="milevox-linux-${architecture}"
  members="${TEMP_DIR}/core-members"
  binary="${TEMP_DIR}/milevox"
  sections="${TEMP_DIR}/sections"
  extracted="${TEMP_DIR}/core"
  readme="${extracted}/${root}/README.md"
  downloader="${TEMP_DIR}/download-model.sh"
  tar --list --gzip --file "${archive}" > "${members}"
  expected=(
    bin/milevox
    install.sh
    uninstall.sh
    scripts/download-model.sh
    scripts/setup-user.sh
    scripts/teardown-user.sh
    scripts/lib-release.sh
    packaging/systemd/milevox.service
    packaging/systemd/environment
    README.md
    LICENSE
    docs/configuration.md
    docs/diagnostics.md
    docs/privacy.md
    docs/release-notes.md
  )
  for member in "${expected[@]}"; do
    require_member "${members}" "${root}/${member}"
  done

  tar --extract --gzip --to-stdout --file "${archive}" \
    "${root}/bin/milevox" > "${binary}"
  readelf --sections --wide "${binary}" > "${sections}"
  if grep -q '[.]symtab' "${sections}"; then
    fail "release binary contains .symtab"
  fi
  mkdir -p -- "${extracted}"
  tar --extract --gzip --file "${archive}" --directory "${extracted}"
  grep -F '660 MB' "${readme}" >/dev/null ||
    fail "release README omits the model download size"
  grep -F 'recognizes English' "${readme}" >/dev/null ||
    fail "release README omits the model language"
  grep -F 'at least 1 GB' "${readme}" >/dev/null ||
    fail "release README omits the free-space requirement"
  verify_readme_links "${extracted}/${root}" "${readme}"
  tar --extract --gzip --to-stdout --file "${archive}" \
    "${root}/scripts/download-model.sh" > "${downloader}"
  grep -F 'about 660 MB' "${downloader}" >/dev/null ||
    fail "release downloader omits the model download size"
  grep -F 'English speech model' "${downloader}" >/dev/null ||
    fail "release downloader omits the model language"
  grep -F 'at least 1 GB' "${downloader}" >/dev/null ||
    fail "release downloader omits the free-space requirement"
}

verify_omarchy_archive() {
  local archive="$1"
  local name
  local version
  local root
  local members
  local extracted
  local member
  local expected

  name="$(basename -- "${archive}")"
  [[ ${name} =~ ^milevox-omarchy-([0-9]+[.][0-9]+[.][0-9]+)\.tar\.gz$ ]] ||
    fail "invalid Omarchy archive name: ${name}"
  version="${BASH_REMATCH[1]}"
  root="milevox-omarchy-${version}"
  members="${TEMP_DIR}/omarchy-members"
  extracted="${TEMP_DIR}/omarchy"
  tar --list --gzip --file "${archive}" > "${members}"
  expected=(
    milevox-omarchy
    install.sh
    uninstall.sh
    bindings-common.sh
    manifest.json
    qmldir
    Panel.qml
    MilevoxOverlay.qml
    MilevoxStatus.qml
    MilevoxStatusLogic.js
    README.md
    LICENSE
  )
  for member in "${expected[@]}"; do
    require_member "${members}" "${root}/${member}"
  done

  mkdir -p -- "${extracted}"
  tar --extract --gzip --file "${archive}" --directory "${extracted}"
  verify_readme_links "${extracted}/${root}" \
    "${extracted}/${root}/README.md"
}

main() {
  local core_archive="${1:-}"
  local omarchy_archive="${2:-}"

  if [[ ${core_archive} == --omarchy-only ]]; then
    [[ -n ${omarchy_archive} && $# -eq 2 ]] ||
      fail "usage: $0 --omarchy-only <omarchy-archive>"
    command -v sha256sum >/dev/null || fail "sha256sum is required"
    command -v tar >/dev/null || fail "tar is required"
    [[ -f ${omarchy_archive} ]] ||
      fail "archive not found: ${omarchy_archive}"
    TEMP_DIR="$(mktemp -d)"
    trap cleanup EXIT
    verify_checksum "${omarchy_archive}"
    verify_omarchy_archive "${omarchy_archive}"
    return
  fi

  [[ -n ${core_archive} && $# -le 2 ]] ||
    fail "usage: $0 <core-archive> [omarchy-archive]"
  command -v readelf >/dev/null || fail "readelf is required"
  command -v sha256sum >/dev/null || fail "sha256sum is required"
  command -v tar >/dev/null || fail "tar is required"
  [[ -f ${core_archive} ]] || fail "archive not found: ${core_archive}"

  TEMP_DIR="$(mktemp -d)"
  trap cleanup EXIT
  verify_checksum "${core_archive}"
  verify_core_archive "${core_archive}"
  if [[ -n ${omarchy_archive} ]]; then
    [[ -f ${omarchy_archive} ]] ||
      fail "archive not found: ${omarchy_archive}"
    verify_checksum "${omarchy_archive}"
    verify_omarchy_archive "${omarchy_archive}"
  fi
}

main "$@"
