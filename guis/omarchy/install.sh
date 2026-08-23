#!/usr/bin/env bash

# Install the Milevox GUI for Omarchy.

set -euo pipefail

readonly PLUGIN_ID="io.github.rselbach.milevox"
readonly BINDINGS_BEGIN="-- milevox:begin"
readonly BINDINGS_END="-- milevox:end"

TEMP_FILES=()

fail() {
  echo "omarchy-gui install: $*" >&2
  exit 1
}

cleanup() {
  local path

  for path in "${TEMP_FILES[@]}"; do
    [[ -n "${path}" ]] && rm -f -- "${path}"
  done
}

usage() {
  cat <<'EOF'
Usage: ./guis/omarchy/install.sh [options]

Prepare Milevox, then install its Omarchy plugin and Hyprland keybindings.
Install Milevox itself from the repository root before installing this GUI.

Options:
  --toggle-key KEY        Toggle binding (default: SUPER + CTRL + X)
  --push-to-talk-key KEY  Push-to-talk binding (default: F9)
  -h, --help              Show this help
EOF
}

prepare_milevox() {
  if command -v milevox-setup >/dev/null; then
    milevox-setup || fail "could not prepare Milevox"
    return
  fi

  milevox status >/dev/null 2>&1 ||
    fail "Milevox is not ready; install it from the repository root first"
}

require_command() {
  local command_name="$1"

  command -v "${command_name}" >/dev/null ||
    fail "${command_name} is required"
}

validate_key() {
  local key="$1"

  [[ -n "${key}" ]] || fail "keybindings cannot be empty"
  [[ "${key}" != *$'\n'* ]] || fail "keybindings cannot contain newlines"
  [[ "${key}" != *'"'* ]] || fail "keybindings cannot contain double quotes"
  [[ "${key}" != *'\\'* ]] || fail "keybindings cannot contain backslashes"
}

remove_bindings_block() {
  local source="$1"
  local destination="$2"

  awk \
    -v begin="${BINDINGS_BEGIN}" \
    -v end="${BINDINGS_END}" \
    '$0 == begin { skipping = 1; next }
     $0 == end { skipping = 0; next }
     !skipping { print }' \
    "${source}" >"${destination}"
}

install_keybindings() {
  local bindings_file="$1"
  local toggle_key="$2"
  local push_to_talk_key="$3"
  local staged_file
  local backup_file
  local instances_json
  local instance_signature
  local reload_output
  local restore_output
  local config_errors
  local validation_error=""

  if [[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]]; then
    instances_json="$(hyprctl instances -j)" ||
      fail "could not discover the running Hyprland instance"
    instance_signature="$(
      jq -r 'if length == 1 then .[0].instance else empty end' \
        <<<"${instances_json}"
    )"
    [[ -n "${instance_signature}" ]] ||
      fail "HYPRLAND_INSTANCE_SIGNATURE is unset;" \
        "expected exactly one running Hyprland instance"
    export HYPRLAND_INSTANCE_SIGNATURE="${instance_signature}"
  fi

  [[ -f "${bindings_file}" ]] ||
    fail "Omarchy bindings file not found: ${bindings_file}"

  staged_file="$(mktemp)"
  backup_file="$(mktemp)"
  TEMP_FILES+=("${staged_file}" "${backup_file}")
  cp -- "${bindings_file}" "${backup_file}"
  remove_bindings_block "${bindings_file}" "${staged_file}"

  printf '\n%s\n' "${BINDINGS_BEGIN}" >>"${staged_file}"
  printf 'hl.unbind("%s")\n' "${toggle_key}" >>"${staged_file}"
  printf 'hl.unbind("%s")\n' "${push_to_talk_key}" >>"${staged_file}"
  printf 'o.bind("%s", "Toggle Milevox dictation", ' \
    "${toggle_key}" >>"${staged_file}"
  printf '"milevox record toggle")\n' >>"${staged_file}"
  printf 'o.bind("%s", "Start Milevox dictation", ' \
    "${push_to_talk_key}" >>"${staged_file}"
  printf '"milevox record start")\n' >>"${staged_file}"
  printf 'o.bind("%s", "Stop Milevox dictation", ' \
    "${push_to_talk_key}" >>"${staged_file}"
  printf '"milevox record stop", { release = true })\n' >>"${staged_file}"
  printf '%s\n' "${BINDINGS_END}" >>"${staged_file}"

  cp -- "${staged_file}" "${bindings_file}"
  config_errors=""
  if ! reload_output="$(hyprctl reload 2>&1)"; then
    validation_error="could not reload Hyprland: ${reload_output}"
  elif ! config_errors="$(hyprctl configerrors 2>&1)"; then
    validation_error="could not inspect Hyprland errors: ${config_errors}"
  elif [[ -n "${config_errors}" ]]; then
    validation_error="Hyprland rejected the keybindings: ${config_errors}"
  fi

  if [[ -n "${validation_error}" ]]; then
    cp -- "${backup_file}" "${bindings_file}"
    if ! restore_output="$(hyprctl reload 2>&1)"; then
      echo "omarchy-gui install: warning: restored ${bindings_file}," \
        "but could not reload Hyprland: ${restore_output}" >&2
    fi
    fail "${validation_error}"
  fi
}

install_plugin() {
  local source_dir="$1"
  local plugin_dir="$2"
  local current_target
  local restart_shell=false

  if [[ -d "${plugin_dir}" && \
    ! -f "${plugin_dir}/MilevoxOverlay.qml" ]]; then
    restart_shell=true
  fi

  if [[ -L "${plugin_dir}" ]]; then
    current_target="$(readlink -f -- "${plugin_dir}")"
    [[ "${current_target}" == "$(readlink -f -- "${source_dir}")" ]] ||
      fail "plugin path is a symlink to another directory: ${plugin_dir}"
    unlink -- "${plugin_dir}"
  fi

  mkdir -p -- "${plugin_dir}"
  install -m 0644 "${source_dir}/manifest.json" \
    "${plugin_dir}/manifest.json"
  install -m 0644 "${source_dir}/Panel.qml" "${plugin_dir}/Panel.qml"
  rm -f -- "${plugin_dir}/Service.qml"
  install -m 0644 "${source_dir}/MilevoxOverlay.qml" \
    "${plugin_dir}/MilevoxOverlay.qml"

  if omarchy-shell shell ping >/dev/null 2>&1; then
    if [[ "${restart_shell}" == true ]]; then
      omarchy restart shell
    else
      omarchy-shell shell rescanPlugins
    fi
    sleep 1
    if ! omarchy plugin list --json \
      | jq -e --arg id "${PLUGIN_ID}" \
        '.[] | select(.id == $id and .enabled)' >/dev/null; then
      omarchy plugin enable "${PLUGIN_ID}" --section right
    fi
  else
    echo "Omarchy Shell is not running; enable ${PLUGIN_ID} after login."
  fi
}

main() {
  local source_dir
  local config_home
  local plugin_dir
  local bindings_file
  local toggle_key="SUPER + CTRL + X"
  local push_to_talk_key="F9"

  while (( $# > 0 )); do
    case "$1" in
      --toggle-key)
        [[ $# -ge 2 ]] || fail "--toggle-key requires a key"
        toggle_key="$2"
        shift 2
        ;;
      --push-to-talk-key)
        [[ $# -ge 2 ]] || fail "--push-to-talk-key requires a key"
        push_to_talk_key="$2"
        shift 2
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

  validate_key "${toggle_key}"
  validate_key "${push_to_talk_key}"
  [[ "${toggle_key}" != "${push_to_talk_key}" ]] ||
    fail "toggle and push-to-talk keys must differ"

  require_command hyprctl
  require_command install
  require_command jq
  require_command omarchy
  require_command omarchy-shell
  require_command milevox

  source_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
  plugin_dir="${config_home}/omarchy/plugins/${PLUGIN_ID}"
  bindings_file="${HOME}/.config/hypr/bindings.lua"

  prepare_milevox
  omarchy plugin validate "${source_dir}"
  install_keybindings \
    "${bindings_file}" "${toggle_key}" "${push_to_talk_key}"
  install_plugin "${source_dir}" "${plugin_dir}"

  echo "Milevox GUI for Omarchy installed."
  echo "Toggle dictation: ${toggle_key}"
  echo "Push to talk: ${push_to_talk_key}"
}

trap cleanup EXIT
main "$@"
