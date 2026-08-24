#!/usr/bin/env bash

# Verify the temporary RustSec allowance and its removal condition.

set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_DIR

fail() {
  echo "audit configuration test: $*" >&2
  exit 1
}

main() {
  local workflow

  workflow="${REPO_DIR}/.github/workflows/audit.yml"
  [[ -f ${workflow} ]] || fail "RustSec workflow is missing"
  grep -Fx '  pull_request:' "${workflow}" >/dev/null ||
    fail "RustSec workflow does not run for pull requests"
  grep -Fx '  schedule:' "${workflow}" >/dev/null ||
    fail "RustSec workflow does not run on a schedule"
  grep -Fx '  workflow_dispatch:' "${workflow}" >/dev/null ||
    fail "RustSec workflow cannot be run manually"
  grep -Fx '  contents: read' "${workflow}" >/dev/null ||
    fail "RustSec workflow permissions are not read-only"
  grep -Fx \
    '        run: cargo install --locked --version 0.22.2 cargo-audit' \
    "${workflow}" >/dev/null ||
    fail "RustSec workflow does not install the pinned scanner"
  grep -Fx '        run: ./scripts/audit-rust.sh' "${workflow}" >/dev/null ||
    fail "RustSec workflow does not use the repository audit policy"
  grep -Fx 'readonly CARGO_AUDIT_VERSION="0.22.2"' \
    "${REPO_DIR}/scripts/audit-rust.sh" >/dev/null ||
    fail "cargo-audit version is not pinned"
  grep -F '"RUSTSEC-2024-0436"' \
    "${REPO_DIR}/.cargo/audit.toml" >/dev/null ||
    fail "paste advisory allowance is missing"
  grep -F 'RUSTSEC-2024-0436' \
    "${REPO_DIR}/docs/release-notes.md" >/dev/null ||
    fail "release notes omit the paste advisory"
  cargo tree -i paste --locked | grep -F 'tokenizers v' >/dev/null ||
    fail "paste no longer has the documented dependency path; remove the allowance"

  echo "Audit configuration tests passed."
}

main "$@"
