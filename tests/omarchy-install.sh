#!/usr/bin/env bash

# Verify the Omarchy installer in an isolated home directory.

set -euo pipefail

readonly EXPECTED_TOGGLE='o.bind("SUPER + CTRL + X", '\
'"Toggle Milevox dictation", '\
'"milevox record toggle", { release = true })'
readonly UNSAFE_TOGGLE='o.bind("SUPER + CTRL + X", '\
'"Toggle Milevox dictation", '\
'"milevox record toggle")'

TEMP_DIR=""

fail() {
  echo "omarchy install test: $*" >&2
  exit 1
}

cleanup() {
  [[ -z "${TEMP_DIR}" ]] || rm -rf -- "${TEMP_DIR}"
}

main() {
  local repo_dir
  local fake_bin
  local test_home
  local bindings_file
  local command_name

  repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
  TEMP_DIR="$(mktemp -d)"
  fake_bin="${TEMP_DIR}/bin"
  test_home="${TEMP_DIR}/home"
  bindings_file="${test_home}/.config/hypr/bindings.lua"

  mkdir -p -- "${fake_bin}" "$(dirname -- "${bindings_file}")"
  printf '%s\n' '-- existing user bindings' >"${bindings_file}"

  install -m 0755 /dev/stdin "${fake_bin}/fake-command" <<'EOF'
#!/usr/bin/env bash

# Stub external commands used by the Omarchy installer.

if [[ "$(basename -- "${0}")" == "omarchy-shell" ]]; then
  exit 1
fi
EOF

  for command_name in hyprctl milevox milevox-setup omarchy omarchy-shell; do
    ln -s -- fake-command "${fake_bin}/${command_name}"
  done

  HOME="${test_home}" \
    XDG_CONFIG_HOME="${test_home}/.config" \
    HYPRLAND_INSTANCE_SIGNATURE="milevox-test" \
    PATH="${fake_bin}:${PATH}" \
    "${repo_dir}/guis/omarchy/install.sh" >/dev/null

  grep -Fqx -- "${EXPECTED_TOGGLE}" "${bindings_file}" ||
    fail "toggle binding does not run on key release"
  if grep -Fqx -- "${UNSAFE_TOGGLE}" "${bindings_file}"; then
    fail "toggle binding still runs while modifiers may be held"
  fi
}

trap cleanup EXIT
main "$@"
