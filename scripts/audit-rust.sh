#!/usr/bin/env bash

# Run the pinned RustSec audit with the repository advisory policy.

set -euo pipefail

readonly CARGO_AUDIT_VERSION="0.22.2"

fail() {
  echo "audit-rust: $*" >&2
  exit 1
}

main() {
  local repo_dir
  local installed_version

  command -v cargo-audit >/dev/null ||
    fail "cargo-audit ${CARGO_AUDIT_VERSION} is required"
  installed_version="$(cargo-audit --version)"
  [[ ${installed_version} == "cargo-audit ${CARGO_AUDIT_VERSION}" ]] ||
    fail "expected cargo-audit ${CARGO_AUDIT_VERSION}, found ${installed_version}"
  repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
  cd -- "${repo_dir}"
  cargo audit --file Cargo.lock
}

main "$@"
