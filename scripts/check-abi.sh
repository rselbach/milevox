#!/usr/bin/env bash

# Enforce the oldest supported runtime ABI and smoke-test a packaged binary.

set -euo pipefail

fail() {
  printf 'check-abi: %s\n' "$*" >&2
  exit 1
}

version_at_most() {
  local actual="$1" limit="$2"
  [[ "$(printf '%s\n%s\n' "${actual}" "${limit}" | sort -V | tail -n 1)" == "${limit}" ]]
}

check_family() {
  local output="$1" family="$2" limit="$3" version
  while IFS= read -r version; do
    [[ -z "${version}" ]] || version_at_most "${version}" "${limit}" ||
      fail "requires ${family}_${version}; maximum is ${family}_${limit}"
  done < <(printf '%s\n' "${output}" | grep -oE "${family}_[0-9]+(\\.[0-9]+)+" |
    sed "s/^${family}_//" | sort -Vu || true)
}

main() {
  local binary="${1:-}" expected_version="${2:-}" output
  [[ $# -ge 1 && $# -le 2 && -x "${binary}" ]] ||
    fail "usage: $0 <executable> [expected-version]"
  command -v "${READELF:-readelf}" >/dev/null || fail "readelf is required"
  output="$("${READELF:-readelf}" --version-info "${binary}")" ||
    fail "could not inspect ${binary}"
  check_family "${output}" GLIBC 2.35
  check_family "${output}" GLIBCXX 3.4.30
  if [[ -n "${expected_version}" ]]; then
    "${binary}" --version | grep -Fqx "milevox ${expected_version}" ||
      fail "binary did not report milevox ${expected_version}"
  else
    "${binary}" --version >/dev/null || fail "binary --version failed"
  fi
}

main "$@"
