#!/usr/bin/env bash

# Install the Milevox CLI and daemon for the current user.

set -euo pipefail

fail() {
  echo "install: $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: ./install.sh [options]

Install the Milevox CLI, speech model, and systemd user service.

Options:
  --skip-model  Do not download or validate the speech model
  -h, --help    Show this help
EOF
}

require_command() {
  local command_name="$1"

  command -v "${command_name}" >/dev/null ||
    fail "${command_name} is required"
}

wait_for_daemon() {
  local binary="$1"
  local attempt
  local status

  # The transcription worker allows up to five minutes for a cold model load.
  for (( attempt = 0; attempt < 3600; attempt++ )); do
    if status="$("${binary}" status 2>/dev/null)"; then
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
  local repo_dir
  local config_home
  local data_home
  local binary_dir
  local systemd_dir
  local binary_source
  local binary_version
  local skip_model=false

  while (( $# > 0 )); do
    case "$1" in
      --skip-model)
        skip_model=true
        shift
        ;;
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

  require_command install
  require_command systemctl
  require_command wl-copy
  require_command wtype

  repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
  data_home="${XDG_DATA_HOME:-${HOME}/.local/share}"
  binary_dir="${HOME}/.local/bin"
  systemd_dir="${config_home}/systemd/user"

  if [[ -x "${binary_dir}/milevox" ]] &&
    "${binary_dir}/milevox" status >/dev/null 2>&1 &&
    ! systemctl --user is-active --quiet milevox.service; then
    fail "stop the manually started Milevox daemon, then run the installer again"
  fi

  if [[ -x "${repo_dir}/bin/milevox" ]]; then
    binary_source="${repo_dir}/bin/milevox"
    echo "Installing the packaged Milevox binary"
  else
    require_command cargo
    echo "Building Milevox"
    cargo build --release --locked \
      --manifest-path "${repo_dir}/Cargo.toml"
    binary_source="${repo_dir}/target/release/milevox"
  fi
  if ! binary_version="$("${binary_source}" --version 2>&1)"; then
    fail "the Milevox binary cannot run on this system: ${binary_version}"
  fi
  echo "${binary_version}"
  install -Dm0755 "${binary_source}" "${binary_dir}/milevox"

  if [[ "${skip_model}" == false ]]; then
    require_command curl
    require_command sha256sum
    "${repo_dir}/scripts/download-model.sh" \
      "${data_home}/milevox/models/parakeet-tdt-0.6b-v2-int8"
  fi

  install -Dm0644 "${repo_dir}/packaging/systemd/milevox.service" \
    "${systemd_dir}/milevox.service"
  install -d -m0700 "${config_home}/milevox"
  if [[ ! -e "${config_home}/milevox/environment" ]]; then
    install -m0600 "${repo_dir}/packaging/systemd/environment" \
      "${config_home}/milevox/environment"
  fi

  systemctl --user daemon-reload
  systemctl --user enable milevox.service
  systemctl --user restart milevox.service
  wait_for_daemon "${binary_dir}/milevox"

  echo "Milevox installed."
}

main "$@"
