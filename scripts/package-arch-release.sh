#!/usr/bin/env bash

# Build a pacman package from a Milevox release archive.

set -euo pipefail
# shellcheck source=scripts/lib-release.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/lib-release.sh"

TEMP_DIR=""

fail() {
  echo "package-arch-release: $*" >&2
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

render_pkgbuild() {
  local template="$1"
  local destination="$2"
  local version="$3"
  local architecture="$4"
  local checksum="$5"

  sed \
    -e "s/@PKGVER@/${version}/g" \
    -e "s/@ARCH@/${architecture}/g" \
    -e "s/@SHA256@/${checksum}/g" \
    "${template}" >"${destination}"
}

main() {
  local package_name="${1:-}"
  local archive_argument="${2:-}"
  local repo_dir
  local dist_dir
  local archive_path
  local archive_name
  local version
  local architecture
  local checksum
  local package_dir
  local source_name
  local template_dir
  local build_architecture
  local build_host
  local makepkg_config

  [[ -n "${package_name}" && -n "${archive_argument}" && $# -eq 2 ]] ||
    fail "usage: $0 <milevox|milevox-omarchy> <release-archive>"

  require_command awk
  require_command cp
  require_command makepkg
  require_command realpath
  require_command sed
  require_command sha256sum

  repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
  dist_dir="${repo_dir}/dist"
  archive_path="$(realpath -- "${archive_argument}")"
  [[ -f "${archive_path}" ]] || fail "archive not found: ${archive_path}"
  archive_name="$(basename -- "${archive_path}")"
  version="$(project_version "${repo_dir}")" || fail "invalid project version"
  checksum="$(sha256sum -- "${archive_path}")"
  checksum="${checksum%% *}"

  case "${package_name}" in
    milevox)
      if [[ "${archive_name}" =~ ^milevox-linux-(x86_64|aarch64)\.tar\.gz$ ]]; then
        architecture="${BASH_REMATCH[1]}"
      else
        fail "invalid Milevox archive name: ${archive_name}"
      fi
      source_name="milevox-${version}-${architecture}.tar.gz"
      ;;
    milevox-omarchy)
      [[ "${archive_name}" == "milevox-omarchy-${version}.tar.gz" ]] ||
        fail "invalid Omarchy archive name: ${archive_name}"
      architecture="any"
      source_name="milevox-omarchy-${version}.tar.gz"
      ;;
    *)
      fail "unknown package: ${package_name}"
      ;;
  esac

  template_dir="${repo_dir}/packaging/arch/${package_name}"
  TEMP_DIR="$(mktemp -d)"
  trap cleanup EXIT
  package_dir="${TEMP_DIR}/${package_name}"
  mkdir -p -- "${package_dir}" "${TEMP_DIR}/src" \
    "${TEMP_DIR}/build" "${TEMP_DIR}/home" "${dist_dir}"

  render_pkgbuild "${template_dir}/PKGBUILD.in" \
    "${package_dir}/PKGBUILD" "${version}" "${architecture}" "${checksum}"
  install -m 0644 "${template_dir}/${package_name}.install" \
    "${package_dir}/${package_name}.install"
  cp -- "${archive_path}" "${TEMP_DIR}/src/${source_name}"

  if [[ "${architecture}" == "any" ]]; then
    build_architecture="$(normalize_architecture "$(uname -m)")"
  else
    build_architecture="${architecture}"
  fi
  case "${build_architecture}" in
    aarch64)
      build_host="aarch64-unknown-linux-gnu"
      ;;
    x86_64)
      build_host="x86_64-pc-linux-gnu"
      ;;
  esac
  makepkg_config="${TEMP_DIR}/makepkg.conf"
  cp -- /etc/makepkg.conf "${makepkg_config}"
  sed -i -E \
    -e "s/^CARCH=.*/CARCH=\"${build_architecture}\"/" \
    -e "s/^CHOST=.*/CHOST=\"${build_host}\"/" \
    "${makepkg_config}"
  cat >>"${makepkg_config}" <<'EOF'

PACKAGER="Roberto Selbach <rselbach@rselbach.com>"
PKGEXT='.pkg.tar.zst'
EOF

  (
    cd -- "${package_dir}"
    HOME="${TEMP_DIR}/home" \
      SRCDEST="${TEMP_DIR}/src" \
      BUILDDIR="${TEMP_DIR}/build" \
      PKGDEST="${dist_dir}" \
      makepkg --config "${makepkg_config}" --cleanbuild --force --nodeps
  )
}

main "$@"
