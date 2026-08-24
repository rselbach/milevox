#!/usr/bin/env bash

set -euo pipefail
# shellcheck source=scripts/lib-release.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/../scripts/lib-release.sh"

for version in 0.0.0 1.2.3 10.20.300; do
  validate_version "${version}" || exit 1
done
for version in '' v1.2.3 1.2 1.2.3.4 01.2.3 1.02.3 1.2.03 1.2.3-rc1 1.2.3+build; do
  ! validate_version "${version}" || exit 1
done
[[ "$(normalize_architecture amd64)" == x86_64 ]]
[[ "$(normalize_architecture x86_64)" == x86_64 ]]
[[ "$(normalize_architecture arm64)" == aarch64 ]]
[[ "$(normalize_architecture aarch64)" == aarch64 ]]
! normalize_architecture riscv64 >/dev/null 2>&1
