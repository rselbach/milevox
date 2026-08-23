#!/usr/bin/env bash

# Remove the Milevox GUI for Omarchy without removing Milevox itself.

set -euo pipefail

readonly PLUGIN_ID="io.github.rselbach.milevox"
readonly BINDINGS_BEGIN="-- milevox:begin"
readonly BINDINGS_END="-- milevox:end"

TEMP_FILES=()

fail() {
  echo "omarchy-gui uninstall: $*" >&2
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
Usage: ./guis/omarchy/uninstall.sh

Remove the Milevox Omarchy plugin and its Hyprland keybindings. The Milevox
CLI, daemon, configuration, credentials, logs, and models are preserved.
EOF
}

configure_hyprland_instance() {
  local instances_json
  local instance_signature

  [[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" ]] || return

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
}

remove_bindings() {
  local bindings_file="$1"
  local staged_file
  local backup_file
  local reload_output
  local restore_output
  local config_errors
  local validation_error=""

  [[ -f "${bindings_file}" ]] || return
  configure_hyprland_instance
  staged_file="$(mktemp)"
  backup_file="$(mktemp)"
  TEMP_FILES+=("${staged_file}" "${backup_file}")
  cp -- "${bindings_file}" "${backup_file}"

  awk \
    -v begin="${BINDINGS_BEGIN}" \
    -v end="${BINDINGS_END}" \
    '$0 == begin { skipping = 1; next }
     $0 == end { skipping = 0; next }
     !skipping { print }' \
    "${bindings_file}" >"${staged_file}"
  cp -- "${staged_file}" "${bindings_file}"

  config_errors=""
  if ! reload_output="$(hyprctl reload 2>&1)"; then
    validation_error="could not reload Hyprland: ${reload_output}"
  elif ! config_errors="$(hyprctl configerrors 2>&1)"; then
    validation_error="could not inspect Hyprland errors: ${config_errors}"
  elif [[ -n "${config_errors}" ]]; then
    validation_error="Hyprland rejected the bindings update: ${config_errors}"
  fi

  if [[ -n "${validation_error}" ]]; then
    cp -- "${backup_file}" "${bindings_file}"
    if ! restore_output="$(hyprctl reload 2>&1)"; then
      echo "omarchy-gui uninstall: warning: restored ${bindings_file}," \
        "but could not reload Hyprland: ${restore_output}" >&2
    fi
    fail "${validation_error}"
  fi
}

remove_plugin() {
  local plugin_dir="$1"

  if omarchy-shell shell ping >/dev/null 2>&1; then
    omarchy plugin disable "${PLUGIN_ID}" >/dev/null 2>&1 || true
  fi

  if [[ -L "${plugin_dir}" ]]; then
    unlink -- "${plugin_dir}"
  elif [[ -d "${plugin_dir}" ]]; then
    rm -f -- "${plugin_dir}/Panel.qml" \
      "${plugin_dir}/MilevoxOverlay.qml" \
      "${plugin_dir}/Service.qml" "${plugin_dir}/manifest.json"
    rmdir -- "${plugin_dir}" 2>/dev/null || true
  fi

  if omarchy-shell shell ping >/dev/null 2>&1; then
    omarchy-shell shell rescanPlugins
  fi
}

main() {
  local config_home
  local plugin_dir

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

  command -v hyprctl >/dev/null || fail "hyprctl is required"
  command -v jq >/dev/null || fail "jq is required"
  command -v omarchy >/dev/null || fail "omarchy is required"
  command -v omarchy-shell >/dev/null || fail "omarchy-shell is required"

  config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
  plugin_dir="${config_home}/omarchy/plugins/${PLUGIN_ID}"

  remove_plugin "${plugin_dir}"
  remove_bindings "${HOME}/.config/hypr/bindings.lua"

  echo "Milevox GUI for Omarchy removed."
  echo "Milevox itself was preserved."
}

trap cleanup EXIT
main "$@"
