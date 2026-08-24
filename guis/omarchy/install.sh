#!/usr/bin/env bash
set -euo pipefail

readonly PLUGIN_ID=io.github.rselbach.milevox
# shellcheck source=guis/omarchy/bindings-common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/bindings-common.sh"

fail() { echo "omarchy-gui install: $*" >&2; exit 1; }
usage() { cat <<'EOF'
Usage: install.sh [--toggle-key KEY] [--push-to-talk-key KEY] [--replace-existing-bindings]

Failure-injection points: plugin-files, bindings, plugin-enabled.
EOF
}
fail_at() { [[ ${MILEVOX_FAIL_AT:-} != "$1" ]] || fail "injected failure after $1"; }

main() {
  local toggle='SUPER + CTRL + X' ptt=F9 replace=false arg source config bindings plugin
  local work stage plugin_stage base after conflicts prior_enabled=false plugin_existed=false
  local plugin_mutated=false committed=false
  while (($#)); do
    arg=$1; shift
    case $arg in
      --toggle-key) (($#)) || fail "--toggle-key requires a key"; toggle=$1; shift ;;
      --push-to-talk-key) (($#)) || fail "--push-to-talk-key requires a key"; ptt=$1; shift ;;
      --replace-existing-bindings) replace=true ;;
      -h|--help) usage; return ;;
      *) fail "unknown option: $arg" ;;
    esac
  done
  ((EUID != 0)) || [[ ${MILEVOX_TEST_ALLOW_ROOT:-} == 1 ]] || fail "run this command without sudo"
  [[ -n $toggle && -n $ptt && $toggle != *$'\n'* && $ptt != *$'\n'* && $toggle != *'"'* && $ptt != *'"'* && $toggle != "$ptt" ]] || fail "invalid keys"
  for arg in hyprctl jq omarchy omarchy-shell milevox; do command -v "$arg" >/dev/null || fail "$arg is required"; done
  source=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
  config=${XDG_CONFIG_HOME:-$HOME/.config}; bindings=$config/hypr/bindings.lua
  plugin=$config/omarchy/plugins/$PLUGIN_ID
  [[ -f $bindings ]] || fail "Omarchy bindings file not found: $bindings"
  validate_bindings_markers "$bindings" || fail "malformed marker block in $bindings"
  [[ ! -L $plugin ]] || fail "plugin path is a conflicting symlink: $plugin"
  [[ ! -e $plugin || -d $plugin ]] || fail "plugin path is not a directory: $plugin"
  omarchy plugin validate "$source" >/dev/null || fail "plugin validation failed"
  conflicts=$(binding_conflicts "$bindings" "$toggle" "$ptt")
  [[ -z $conflicts || $replace == true ]] || fail "requested key is already bound outside Milevox block: $conflicts"
  if [[ -z ${HYPRLAND_INSTANCE_SIGNATURE:-} ]]; then
    export HYPRLAND_INSTANCE_SIGNATURE
    HYPRLAND_INSTANCE_SIGNATURE=$(hyprctl instances -j | jq -r 'if length == 1 then .[0].instance else empty end')
    [[ -n $HYPRLAND_INSTANCE_SIGNATURE ]] || fail "cannot choose Hyprland instance (set HYPRLAND_INSTANCE_SIGNATURE or run exactly one instance)"
  fi
  base=$(hyprctl configerrors 2>&1) || fail "could not read Hyprland config errors"
  command -v milevox-setup >/dev/null && milevox-setup >/dev/null || milevox status >/dev/null 2>&1 || fail "Milevox is not ready"
  mkdir -p -- "$(dirname -- "$plugin")"
  work=$(mktemp -d "$(dirname -- "$plugin")/.milevox-txn.XXXXXX")
  plugin_stage=$work/staged-plugin
  stage=$(stage_for "$bindings"); cp -p -- "$bindings" "$work/bindings"
  mkdir -p -- "$plugin_stage"
  if [[ -e $plugin ]]; then
    plugin_existed=true
    cp -a -- "$plugin/." "$plugin_stage/"
  fi
  if omarchy plugin list --json 2>/dev/null | jq -e --arg id "$PLUGIN_ID" '.[]|select(.id==$id and .enabled)' >/dev/null; then prior_enabled=true; fi
  rollback() {
    [[ $committed == false ]] || return 0
    if cp -p -- "$work/bindings" "$stage"; then atomic_replace "$stage" "$bindings" || true; fi
    if [[ $plugin_mutated == true ]]; then
      rm -rf -- "$plugin"
      [[ $plugin_existed == false ]] || mv -- "$work/live-plugin" "$plugin"
    fi
    if omarchy-shell shell ping >/dev/null 2>&1; then
      if [[ $prior_enabled == true ]]; then omarchy plugin enable "$PLUGIN_ID" --section right >/dev/null 2>&1 || true; else omarchy plugin disable "$PLUGIN_ID" >/dev/null 2>&1 || true; fi
      hyprctl reload >/dev/null 2>&1 || true
    fi
    rm -rf -- "$work"; rm -f -- "$stage"
  }
  trap rollback EXIT
  rm -f -- "$plugin_stage/manifest.json" "$plugin_stage/Panel.qml" \
    "$plugin_stage/MilevoxOverlay.qml" "$plugin_stage/MilevoxStatus.qml" \
    "$plugin_stage/Service.qml"
  for arg in manifest.json Panel.qml MilevoxOverlay.qml MilevoxStatus.qml; do install -m 0644 "$source/$arg" "$plugin_stage/$arg"; done
  if [[ $plugin_existed == true ]]; then
    mv -- "$plugin" "$work/live-plugin"
    plugin_mutated=true
  else
    plugin_mutated=true
  fi
  mv -- "$plugin_stage" "$plugin"
  fail_at plugin-files
  if [[ $replace == true ]]; then remove_binding_conflicts "$bindings" "$toggle" "$ptt" > "$stage"; else strip_bindings_block "$bindings" > "$stage"; fi
  append_binding_block "$stage" "$toggle" "$ptt"
  atomic_replace "$stage" "$bindings"; fail_at bindings
  hyprctl reload >/dev/null || fail "could not reload Hyprland"
  after=$(hyprctl configerrors 2>&1) || fail "could not validate Hyprland configuration"
  [[ -z $(new_config_errors "$base" "$after") ]] || fail "Hyprland reported new configuration errors"
  if omarchy-shell shell ping >/dev/null 2>&1; then omarchy-shell shell rescanPlugins >/dev/null; omarchy plugin enable "$PLUGIN_ID" --section right >/dev/null; fi
  fail_at plugin-enabled
  committed=true; trap - EXIT; rm -rf -- "$work"; echo "Milevox GUI for Omarchy installed."
}
main "$@"
