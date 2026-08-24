#!/usr/bin/env bash

# Exercise Omarchy install/uninstall transactions in an isolated desktop home.

set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_DIR
readonly PLUGIN_ID="io.github.rselbach.milevox"
TEMP_DIR=""
CASE_DIR=""
FAKE_BIN=""
TEST_HOME=""
TEST_XDG=""
STATE_DIR=""
BINDINGS=""
LEGACY_BINDINGS=""
PLUGIN=""

fail() {
  printf 'omarchy transaction test: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  [[ -z $TEMP_DIR ]] || rm -rf -- "$TEMP_DIR"
}

new_case() {
  local name=$1 command_name
  CASE_DIR=$TEMP_DIR/$name
  FAKE_BIN=$CASE_DIR/bin
  TEST_HOME=$CASE_DIR/home
  TEST_XDG=$CASE_DIR/xdg
  STATE_DIR=$CASE_DIR/state
  BINDINGS=$TEST_XDG/hypr/bindings.lua
  LEGACY_BINDINGS=$TEST_HOME/.config/hypr/bindings.lua
  PLUGIN=$TEST_XDG/omarchy/plugins/$PLUGIN_ID
  mkdir -p -- "$FAKE_BIN" "$(dirname -- "$BINDINGS")" "$STATE_DIR"
  printf '%s\n' '-- existing user bindings' > "$BINDINGS"
  chmod 0640 "$BINDINGS"
  printf '%s\n' '[{"instance":"milevox-test"}]' > "$STATE_DIR/instances.json"
  printf '%s\n' false > "$STATE_DIR/enabled"
  : > "$STATE_DIR/baseline-errors"
  touch "$STATE_DIR/online"

  install -m 0755 /dev/stdin "$FAKE_BIN/fake-command" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
name=$(basename -- "$0")
state=${MILEVOX_TEST_STATE:?}
printf '%s %s\n' "$name" "$*" >> "$state/commands.log"
case $name in
  hyprctl)
    case ${1:-} in
      instances) cat "$state/instances.json" ;;
      configerrors)
        cat "$state/baseline-errors"
        if [[ -f $state/reloaded && -f $state/new-errors ]]; then cat "$state/new-errors"; fi
        ;;
      reload)
        [[ ! -f $state/fail-reload ]] || exit 1
        touch "$state/reloaded"
        ;;
      *) exit 1 ;;
    esac
    ;;
  omarchy)
    case "${1:-} ${2:-}" in
      'plugin validate') [[ ! -f $state/fail-plugin-validation ]] ;;
      'plugin list') printf '[{"id":"io.github.rselbach.milevox","enabled":%s}]\n' "$(cat "$state/enabled")" ;;
      'plugin enable') printf '%s\n' true > "$state/enabled" ;;
      'plugin disable') printf '%s\n' false > "$state/enabled" ;;
      *) exit 1 ;;
    esac
    ;;
  omarchy-shell)
    case "${1:-} ${2:-}" in
      'shell ping') [[ -f $state/online ]] ;;
      'shell rescanPlugins') [[ -f $state/online ]] ;;
      *) exit 1 ;;
    esac
    ;;
  milevox|milevox-setup) ;;
  *) exit 1 ;;
esac
EOF
  for command_name in hyprctl milevox milevox-setup omarchy omarchy-shell; do
    ln -s -- fake-command "$FAKE_BIN/$command_name"
  done
}

run_install() {
  env HOME="$TEST_HOME" XDG_CONFIG_HOME="$TEST_XDG" \
    HYPRLAND_INSTANCE_SIGNATURE=milevox-test MILEVOX_TEST_ALLOW_ROOT=1 \
    MILEVOX_TEST_STATE="$STATE_DIR" PATH="$FAKE_BIN:/usr/bin:/bin" \
    MILEVOX_FAIL_AT="${MILEVOX_FAIL_AT:-}" \
    "$REPO_DIR/guis/omarchy/install.sh" "$@"
}

run_install_discovering_instance() {
  env -u HYPRLAND_INSTANCE_SIGNATURE HOME="$TEST_HOME" \
    XDG_CONFIG_HOME="$TEST_XDG" MILEVOX_TEST_ALLOW_ROOT=1 \
    MILEVOX_TEST_STATE="$STATE_DIR" PATH="$FAKE_BIN:/usr/bin:/bin" \
    "$REPO_DIR/guis/omarchy/install.sh"
}

run_uninstall() {
  env -u HYPRLAND_INSTANCE_SIGNATURE HOME="$TEST_HOME" \
    XDG_CONFIG_HOME="$TEST_XDG" MILEVOX_TEST_ALLOW_ROOT=1 \
    MILEVOX_TEST_STATE="$STATE_DIR" PATH="$FAKE_BIN:/usr/bin:/bin" \
    MILEVOX_FAIL_AT="${MILEVOX_FAIL_AT:-}" \
    "$REPO_DIR/guis/omarchy/uninstall.sh"
}

snapshot_state() {
  local destination=$1
  mkdir -p -- "$destination"
  if [[ -f $BINDINGS ]]; then
    touch "$destination/bindings-present"
    cp -p -- "$BINDINGS" "$destination/bindings"
  fi
  if [[ -f $LEGACY_BINDINGS ]]; then
    touch "$destination/legacy-present"
    cp -p -- "$LEGACY_BINDINGS" "$destination/legacy"
  fi
  if [[ -L $PLUGIN ]]; then
    touch "$destination/plugin-symlink"
    readlink -- "$PLUGIN" > "$destination/plugin-target"
  elif [[ -d $PLUGIN ]]; then
    touch "$destination/plugin-directory"
    cp -a -- "$PLUGIN" "$destination/plugin"
  fi
  cat "$STATE_DIR/enabled" > "$destination/enabled"
}

assert_state_matches() {
  local expected=$1
  if [[ -f $expected/bindings-present ]]; then
    [[ -f $BINDINGS ]] || fail "bindings disappeared during rollback"
    cmp -- "$expected/bindings" "$BINDINGS" || fail "bindings bytes changed during rollback"
    [[ $(stat -c %a -- "$expected/bindings") == "$(stat -c %a -- "$BINDINGS")" ]] ||
      fail "bindings mode changed during rollback"
  else
    [[ ! -e $BINDINGS ]] || fail "rollback created a bindings file"
  fi
  if [[ -f $expected/legacy-present ]]; then
    [[ -f $LEGACY_BINDINGS ]] || fail "legacy bindings disappeared during rollback"
    cmp -- "$expected/legacy" "$LEGACY_BINDINGS" || fail "legacy bindings changed during rollback"
    [[ $(stat -c %a -- "$expected/legacy") == "$(stat -c %a -- "$LEGACY_BINDINGS")" ]] ||
      fail "legacy bindings mode changed during rollback"
  else
    [[ ! -e $LEGACY_BINDINGS ]] || fail "rollback created legacy bindings"
  fi
  if [[ -f $expected/plugin-directory ]]; then
    [[ -d $PLUGIN && ! -L $PLUGIN ]] || fail "plugin directory was not restored"
    diff -r -- "$expected/plugin" "$PLUGIN" >/dev/null || fail "plugin files changed during rollback"
  elif [[ -f $expected/plugin-symlink ]]; then
    [[ -L $PLUGIN ]] || fail "plugin symlink was not restored"
    [[ $(readlink -- "$PLUGIN") == "$(cat "$expected/plugin-target")" ]] ||
      fail "plugin symlink target changed during rollback"
  else
    [[ ! -e $PLUGIN && ! -L $PLUGIN ]] || fail "rollback left a plugin behind"
  fi
  cmp -- "$expected/enabled" "$STATE_DIR/enabled" || fail "plugin enabled state changed during rollback"
}

seed_old_plugin() {
  mkdir -p -- "$PLUGIN"
  printf '%s\n' 'old panel' > "$PLUGIN/Panel.qml"
  printf '%s\n' 'preserve unknown plugin file' > "$PLUGIN/user-extra"
  printf '%s\n' 'obsolete owned file' > "$PLUGIN/Service.qml"
}

seed_legacy_managed_bindings() {
  mkdir -p -- "$(dirname -- "$LEGACY_BINDINGS")"
  printf '%s\n' '-- legacy user binding' '-- milevox:begin' \
    'o.bind("F8", "Milevox", "milevox record toggle")' \
    '-- milevox:end' > "$LEGACY_BINDINGS"
  chmod 0600 "$LEGACY_BINDINGS"
}

assert_clean_transaction_files() {
  if find "$TEST_XDG/omarchy/plugins" "$TEST_XDG/hypr" \
    -maxdepth 1 -name '.milevox*' -print 2>/dev/null | grep -q .; then
    fail "transaction temporary files were left behind"
  fi
}

test_normal_install_and_upgrade() {
  local first
  new_case normal-install
  run_install >/dev/null
  for file in manifest.json Panel.qml MilevoxOverlay.qml MilevoxStatus.qml; do
    [[ -f $PLUGIN/$file ]] || fail "install omitted $file"
  done
  [[ $(stat -c %a -- "$BINDINGS") == 640 ]] || fail "install changed bindings mode"
  [[ $(grep -Fxc -- '-- milevox:begin' "$BINDINGS") == 1 ]] || fail "install wrote duplicate markers"
  grep -Fqx -- 'o.bind("SUPER + CTRL + X", "Toggle Milevox dictation", "milevox record toggle", { release = true })' "$BINDINGS" ||
    fail "toggle does not execute on key release"
  [[ $(cat "$STATE_DIR/enabled") == true ]] || fail "install did not enable plugin"
  first=$CASE_DIR/first-bindings
  cp -- "$BINDINGS" "$first"
  run_install >/dev/null
  cmp -- "$first" "$BINDINGS" || fail "repeated install changed managed bindings"
  [[ $(grep -Fxc -- '-- milevox:begin' "$BINDINGS") == 1 ]] || fail "upgrade duplicated markers"
  assert_clean_transaction_files
}

test_install_preflight_failures() {
  local before malformed index=0
  new_case missing-bindings
  rm -- "$BINDINGS"
  if run_install >/dev/null 2>&1; then fail "install accepted a missing bindings file"; fi
  [[ ! -e $PLUGIN ]] || fail "missing-bindings failure installed plugin files"

  new_case shortcut-conflict
  printf '%s\n' 'o.bind("F9", "Other action", "other-command")' >> "$BINDINGS"
  before=$CASE_DIR/before; cp -p -- "$BINDINGS" "$before"
  : > "$STATE_DIR/commands.log"
  if run_install >/dev/null 2>&1; then fail "install replaced a conflicting shortcut"; fi
  cmp -- "$before" "$BINDINGS" || fail "shortcut conflict changed bindings"
  [[ ! -e $PLUGIN ]] || fail "shortcut conflict installed plugin files"
  ! grep -q '^hyprctl reload' "$STATE_DIR/commands.log" || fail "shortcut conflict reloaded Hyprland"
  run_install --replace-existing-bindings >/dev/null
  ! grep -Fq -- 'o.bind("F9", "Other action"' "$BINDINGS" || fail "explicit replacement kept conflict"

  new_case plugin-symlink-conflict
  mkdir -p -- "$CASE_DIR/other" "$(dirname -- "$PLUGIN")"
  ln -s -- "$CASE_DIR/other" "$PLUGIN"
  before=$CASE_DIR/before; cp -p -- "$BINDINGS" "$before"
  if run_install >/dev/null 2>&1; then fail "install accepted a conflicting plugin symlink"; fi
  [[ -L $PLUGIN ]] || fail "plugin symlink conflict removed the symlink"
  cmp -- "$before" "$BINDINGS" || fail "plugin symlink conflict changed bindings"

  for malformed in \
    $'-- user\n-- milevox:begin\ninside\n' \
    $'-- user\n-- milevox:end\n' \
    $'-- milevox:end\n-- milevox:begin\n' \
    $'-- milevox:begin\n-- milevox:begin\n-- milevox:end\n' \
    $'-- milevox:begin\n-- milevox:end\n-- milevox:begin\n-- milevox:end\n'; do
    index=$((index + 1))
    new_case "malformed-$index"
    printf '%s' "$malformed" > "$BINDINGS"
    before=$CASE_DIR/before; cp -p -- "$BINDINGS" "$before"
    if run_install >/dev/null 2>&1; then fail "install accepted malformed marker case $index"; fi
    cmp -- "$before" "$BINDINGS" || fail "malformed marker case $index changed bytes"
    [[ ! -e $PLUGIN ]] || fail "malformed marker case $index installed plugin"
  done

  new_case multiple-instances-install
  printf '%s\n' '[{"instance":"one"},{"instance":"two"}]' > "$STATE_DIR/instances.json"
  before=$CASE_DIR/before; cp -p -- "$BINDINGS" "$before"
  if run_install_discovering_instance >/dev/null 2>&1; then fail "install guessed between Hyprland instances"; fi
  cmp -- "$before" "$BINDINGS" || fail "multiple-instance failure changed bindings"
}

test_install_rollbacks() {
  local point snapshot
  for point in plugin-files bindings plugin-enabled; do
    new_case "install-failure-$point"
    seed_old_plugin
    snapshot=$CASE_DIR/snapshot
    snapshot_state "$snapshot"
    if MILEVOX_FAIL_AT=$point run_install >/dev/null 2>&1; then
      fail "install failure point $point did not fail"
    fi
    assert_state_matches "$snapshot"
    assert_clean_transaction_files
  done

  for point in reload new-config-error; do
    new_case "install-validation-$point"
    seed_old_plugin
    snapshot=$CASE_DIR/snapshot
    snapshot_state "$snapshot"
    if [[ $point == reload ]]; then touch "$STATE_DIR/fail-reload"; else printf '%s\n' 'new Milevox error' > "$STATE_DIR/new-errors"; fi
    if run_install >/dev/null 2>&1; then fail "install accepted $point"; fi
    assert_state_matches "$snapshot"
  done
}

test_uninstall_and_offline_paths() {
  local output
  new_case normal-uninstall
  run_install >/dev/null
  seed_legacy_managed_bindings
  run_uninstall >/dev/null
  ! grep -Fq -- '-- milevox:' "$BINDINGS" || fail "uninstall kept primary markers"
  ! grep -Fq -- '-- milevox:' "$LEGACY_BINDINGS" || fail "uninstall kept legacy markers"
  [[ ! -e $PLUGIN ]] || fail "uninstall kept the owned plugin"
  [[ $(cat "$STATE_DIR/enabled") == false ]] || fail "uninstall kept plugin enabled"
  run_uninstall >/dev/null

  new_case offline-uninstall
  run_install >/dev/null
  rm -- "$FAKE_BIN/hyprctl"
  output=$(run_uninstall)
  [[ $output == *offline* ]] || fail "offline uninstall omitted relogin notice"
  [[ ! -e $PLUGIN ]] || fail "offline uninstall kept plugin"
  ! grep -Fq -- '-- milevox:' "$BINDINGS" || fail "offline uninstall kept bindings"

  new_case multiple-instances-uninstall
  run_install >/dev/null
  printf '%s\n' '[{"instance":"one"},{"instance":"two"}]' > "$STATE_DIR/instances.json"
  output=$(run_uninstall)
  [[ $output == *offline* ]] || fail "multiple-instance uninstall guessed a running instance"
  [[ ! -e $PLUGIN ]] || fail "multiple-instance uninstall kept plugin files"

  new_case missing-bindings-uninstall
  seed_old_plugin
  rm -- "$BINDINGS"
  run_uninstall >/dev/null
  [[ ! -e $PLUGIN/Panel.qml && ! -e $PLUGIN/Service.qml ]] ||
    fail "uninstall with missing bindings kept owned plugin files"
  [[ -f $PLUGIN/user-extra ]] || fail "uninstall removed an unowned plugin file"
  run_uninstall >/dev/null
}

test_uninstall_preflight_and_rollbacks() {
  local point snapshot before
  new_case malformed-uninstall
  seed_old_plugin
  printf '%s\n' '-- milevox:end' > "$BINDINGS"
  before=$CASE_DIR/before; snapshot_state "$before"
  if run_uninstall >/dev/null 2>&1; then fail "uninstall accepted malformed markers"; fi
  assert_state_matches "$before"

  for point in plugin-disabled plugin-files bindings legacy-bindings; do
    new_case "uninstall-failure-$point"
    run_install >/dev/null
    seed_legacy_managed_bindings
    snapshot=$CASE_DIR/snapshot
    snapshot_state "$snapshot"
    if MILEVOX_FAIL_AT=$point run_uninstall >/dev/null 2>&1; then
      fail "uninstall failure point $point did not fail"
    fi
    assert_state_matches "$snapshot"
    assert_clean_transaction_files
  done
}

main() {
  TEMP_DIR=$(mktemp -d)
  "$REPO_DIR/guis/omarchy/install.sh" --help >/dev/null
  "$REPO_DIR/guis/omarchy/uninstall.sh" --help >/dev/null
  test_normal_install_and_upgrade
  test_install_preflight_failures
  test_install_rollbacks
  test_uninstall_and_offline_paths
  test_uninstall_preflight_and_rollbacks
  printf '%s\n' 'Omarchy transaction tests passed.'
}

trap cleanup EXIT
main "$@"
