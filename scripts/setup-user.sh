#!/usr/bin/env bash

# Install Milevox's per-user model and start its systemd service.

set -euo pipefail

fail() {
  echo "milevox-setup: $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: milevox-setup

Install or validate the speech model, then enable and start the Milevox user
service. Run this command as the desktop user, without sudo.
EOF
}

require_command() {
  local command_name="$1"

  command -v "${command_name}" >/dev/null ||
    fail "${command_name} is required"
}

wait_for_daemon() {
  local attempt

  for (( attempt = 0; attempt < 100; attempt++ )); do
    if milevox status >/dev/null 2>&1; then
      return
    fi
    sleep 0.1
  done

  systemctl --user status milevox.service --no-pager >&2 || true
  fail "Milevox did not become ready"
}

main() {
  local was_recording=false
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

  (( EUID != 0 )) || fail "run this command without sudo"
  require_command milevox
  require_command milevox-download-model
  require_command systemctl

  if milevox status >/dev/null 2>&1 &&
    ! systemctl --user is-active --quiet milevox.service; then
    fail "stop the manually running Milevox daemon first"
  fi
  if milevox status 2>/dev/null | grep -qi recording; then
    was_recording=true
  fi
  milevox-download-model
  systemctl --user daemon-reload ||
    fail "could not reload the systemd user manager"
  systemctl --user enable milevox.service || fail "could not enable milevox.service"
  systemctl --user restart milevox.service || fail "could not restart milevox.service"
  wait_for_daemon

  [[ "$was_recording" == false ]] || echo "Warning: the active recording was interrupted." >&2

  echo "Milevox is ready."
}

main "$@"
