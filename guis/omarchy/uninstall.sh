#!/usr/bin/env bash

# Remove the Milevox Omarchy plugin and managed keybindings.

set -euo pipefail
readonly PLUGIN_ID=io.github.rselbach.milevox
# shellcheck source=guis/omarchy/bindings-common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/bindings-common.sh"
fail() { echo "omarchy-gui uninstall: $*" >&2; exit 1; }
usage() { printf '%s\n' 'Usage: uninstall.sh [--help]' 'Failure-injection points: plugin-disabled, plugin-files, bindings, legacy-bindings.'; }
fail_at() { [[ ${MILEVOX_FAIL_AT:-} != "$1" ]] || fail "injected failure after $1"; }

main() {
  local config plugin bindings legacy work online=false prior_enabled=false plugin_existed=false committed=false
  local stage legacy_stage='' base='' after
  while (($#)); do case $1 in -h|--help) usage; return;; *) fail "unknown option: $1";; esac; done
  ((EUID != 0)) || [[ ${MILEVOX_TEST_ALLOW_ROOT:-} == 1 ]] || fail "run this command without sudo"
  config=${XDG_CONFIG_HOME:-$HOME/.config}; plugin=$config/omarchy/plugins/$PLUGIN_ID
  bindings=$config/hypr/bindings.lua; legacy=$HOME/.config/hypr/bindings.lua
  [[ ! -e $plugin || -d $plugin || -L $plugin ]] || fail "plugin path is not a directory or symlink: $plugin"
  [[ ! -f $bindings ]] || validate_bindings_markers "$bindings" || fail "malformed marker block in $bindings"
  if [[ $legacy != "$bindings" && -f $legacy ]]; then validate_bindings_markers "$legacy" || fail "malformed marker block in $legacy"; fi
  if command -v hyprctl >/dev/null && command -v jq >/dev/null; then
    if [[ -n ${HYPRLAND_INSTANCE_SIGNATURE:-} ]]; then online=true
    else
      HYPRLAND_INSTANCE_SIGNATURE=$(hyprctl instances -j 2>/dev/null | jq -r 'if length == 1 then .[0].instance else empty end') || true
      [[ -z $HYPRLAND_INSTANCE_SIGNATURE ]] || { export HYPRLAND_INSTANCE_SIGNATURE; online=true; }
    fi
  fi
  [[ $online == false ]] || base=$(hyprctl configerrors 2>&1) || fail "could not read Hyprland config errors"
  work=$(mktemp -d); [[ ! -e $plugin && ! -L $plugin ]] || { plugin_existed=true; cp -a -- "$plugin" "$work/plugin"; }
  [[ ! -f $bindings ]] || cp -p -- "$bindings" "$work/bindings"
  [[ $legacy == "$bindings" || ! -f $legacy ]] || cp -p -- "$legacy" "$work/legacy"
  if command -v omarchy >/dev/null && omarchy plugin list --json 2>/dev/null | jq -e --arg id "$PLUGIN_ID" '.[]|select(.id==$id and .enabled)' >/dev/null; then prior_enabled=true; fi
  rollback() {
    [[ $committed == false ]] || return 0
    if [[ -f $work/bindings ]]; then stage=$(stage_for "$bindings"); cp -p "$work/bindings" "$stage"; atomic_replace "$stage" "$bindings" || true; fi
    if [[ -f $work/legacy ]]; then legacy_stage=$(stage_for "$legacy"); cp -p "$work/legacy" "$legacy_stage"; atomic_replace "$legacy_stage" "$legacy" || true; fi
    rm -rf -- "$plugin"; [[ $plugin_existed == false ]] || cp -a -- "$work/plugin" "$plugin"
    if [[ $online == true && $prior_enabled == true ]]; then omarchy plugin enable "$PLUGIN_ID" --section right >/dev/null 2>&1 || true; fi
    [[ $online == false ]] || hyprctl reload >/dev/null 2>&1 || true
    rm -rf -- "$work"
  }
  trap rollback EXIT
  if [[ $online == true && $prior_enabled == true ]]; then omarchy plugin disable "$PLUGIN_ID" >/dev/null; fail_at plugin-disabled; fi
  if [[ -L $plugin ]]; then unlink -- "$plugin"; elif [[ -d $plugin ]]; then
    rm -f -- "$plugin/manifest.json" "$plugin/qmldir" "$plugin/Panel.qml" \
      "$plugin/MilevoxOverlay.qml" "$plugin/MilevoxStatus.qml" \
      "$plugin/MilevoxStatusLogic.js" "$plugin/Service.qml"
    rmdir -- "$plugin" 2>/dev/null || true
  fi
  fail_at plugin-files
  if [[ -f $bindings ]]; then stage=$(stage_for "$bindings"); strip_bindings_block "$bindings" > "$stage"; atomic_replace "$stage" "$bindings"; fail_at bindings; fi
  if [[ $legacy != "$bindings" && -f $legacy ]]; then legacy_stage=$(stage_for "$legacy"); strip_bindings_block "$legacy" > "$legacy_stage"; atomic_replace "$legacy_stage" "$legacy"; fail_at legacy-bindings; fi
  if [[ $online == true ]]; then
    hyprctl reload >/dev/null || fail "could not reload Hyprland"
    after=$(hyprctl configerrors 2>&1) || fail "could not validate Hyprland configuration"
    [[ -z $(new_config_errors "$base" "$after") ]] || fail "Hyprland reported new configuration errors"
    if command -v omarchy-shell >/dev/null; then omarchy-shell shell rescanPlugins >/dev/null || true; fi
  else echo "Hyprland/Omarchy is offline; log out and back in to apply removal."; fi
  committed=true; trap - EXIT; rm -rf -- "$work"; echo "Milevox GUI for Omarchy removed."
}
main "$@"
