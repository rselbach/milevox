#!/usr/bin/env bash

# Remove the Milevox CLI and daemon while preserving user data.

set -euo pipefail

fail() {
  echo "uninstall: $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: ./uninstall.sh

Remove the Milevox CLI and systemd user service. Configuration, credentials,
logs, and speech models are preserved.
EOF
}

main() {
  local config_home
  local service_path

  while (( $# > 0 )); do
    case "$1" in
      -h | --help)
        usage
        return
        ;;
      *)
        fail "unknown option: $1"
        ;;
    esac
  done

  command -v systemctl >/dev/null || fail "systemctl is required"

  config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
  service_path="${config_home}/systemd/user/milevox.service"

  if [[ -x "${HOME}/.local/bin/milevox" ]] &&
    "${HOME}/.local/bin/milevox" status >/dev/null 2>&1 &&
    ! systemctl --user is-active --quiet milevox.service; then
    fail "stop the manually started Milevox daemon, then run again"
  fi
  if [[ -e "${service_path}" ]]; then
    systemctl --user disable --now milevox.service >/dev/null ||
      fail "could not stop and disable milevox.service"
  fi
  rm -f -- "${service_path}"
  systemctl --user daemon-reload
  rm -f -- "${HOME}/.local/bin/milevox"

  echo "Milevox removed."
  echo "Configuration, credentials, logs, and models were preserved."
}

main "$@"
