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
  local status

  # The transcription worker allows up to five minutes for a cold model load.
  for (( attempt = 0; attempt < 3600; attempt++ )); do
    if status="$(milevox status 2>/dev/null)"; then
      if grep -Eq '"state"[[:space:]]*:[[:space:]]*"idle"' <<<"${status}"; then
        return
      fi
      if grep -Eq '"code"[[:space:]]*:[[:space:]]*"model_unavailable"' \
        <<<"${status}" ||
        grep -Eq '"state"[[:space:]]*:[[:space:]]*"error"' <<<"${status}"; then
        printf '%s\n' "${status}" >&2
        fail "Milevox model is unavailable"
      fi
    fi
    sleep 0.1
  done

  systemctl --user status milevox.service --no-pager >&2 || true
  fail "Milevox did not become ready"
}

main() {
  local config_home
  local environment_file
  local environment_source
  local status
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
  require_command install
  require_command systemctl

  status=""
  if status="$(milevox status 2>/dev/null)"; then
    if ! systemctl --user is-active --quiet milevox.service; then
      fail "stop the manually running Milevox daemon first"
    fi
    if grep -Eq '"state"[[:space:]]*:[[:space:]]*"(recording|transcribing|refining|canceling)"' \
      <<<"${status}"; then
      fail "Milevox is busy; wait for it to become idle or cancel the active dictation"
    fi
  fi

  config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
  environment_file="${config_home}/milevox/environment"
  environment_source="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/packaging/systemd/environment"
  if [[ ! -f "${environment_source}" ]]; then
    environment_source="/usr/share/doc/milevox/environment.example"
  fi
  install -d -m0700 "${config_home}/milevox"
  if [[ ! -e "${environment_file}" ]]; then
    [[ -f "${environment_source}" ]] || fail "service environment example not found"
    install -m0600 "${environment_source}" "${environment_file}"
  fi

  milevox-download-model
  systemctl --user daemon-reload ||
    fail "could not reload the systemd user manager"
  systemctl --user enable milevox.service || fail "could not enable milevox.service"
  systemctl --user restart milevox.service || fail "could not restart milevox.service"
  wait_for_daemon

  echo "Milevox is ready."
}

main "$@"
