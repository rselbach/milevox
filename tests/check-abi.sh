#!/usr/bin/env bash

set -euo pipefail
tmp="$(mktemp -d)"
trap 'rm -rf -- "${tmp}"' EXIT
cat >"${tmp}/binary" <<'EOF'
#!/usr/bin/env bash
printf 'milevox 1.2.3\n'
EOF
cat >"${tmp}/readelf" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "${READELF_OUTPUT}"
EOF
chmod +x "${tmp}/binary" "${tmp}/readelf"
READELF="${tmp}/readelf" READELF_OUTPUT='GLIBC_2.35 GLIBCXX_3.4.30' \
  ./scripts/check-abi.sh "${tmp}/binary" 1.2.3
if READELF="${tmp}/readelf" READELF_OUTPUT='GLIBC_2.36' \
  ./scripts/check-abi.sh "${tmp}/binary" >/dev/null 2>&1; then
  exit 1
fi
if READELF="${tmp}/readelf" READELF_OUTPUT='GLIBCXX_3.4.9 GLIBCXX_3.4.31' \
  ./scripts/check-abi.sh "${tmp}/binary" >/dev/null 2>&1; then
  exit 1
fi
