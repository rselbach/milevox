#!/usr/bin/env bash

# Verify package and raw per-user lifecycle ownership without a real systemd session.

set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_DIR
TEMP_DIR=""

fail() {
  printf 'package lifecycle test: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  [[ -z $TEMP_DIR ]] || rm -rf -- "$TEMP_DIR"
}

new_home() {
  local name=$1 command_name
  HOME_DIR=$TEMP_DIR/$name/home
  STATE_DIR=$TEMP_DIR/$name/state
  BIN_DIR=$TEMP_DIR/$name/bin
  mkdir -p -- "$HOME_DIR/.local/bin" "$HOME_DIR/.config/systemd/user" \
    "$STATE_DIR" "$BIN_DIR"
  printf '%s\n' not-found > "$STATE_DIR/load"
  : > "$STATE_DIR/fragment"
  printf '%s\n' inactive > "$STATE_DIR/active"
  : > "$STATE_DIR/commands"

  install -m 0755 /dev/stdin "$BIN_DIR/fake-command" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
name=$(basename -- "$0")
state=${MILEVOX_TEST_STATE:?}
printf '%s %s\n' "$name" "$*" >> "$state/commands"
case $name in
  systemctl)
    shift
    case ${1:-} in
      is-active) [[ $(cat "$state/active") == active ]] ;;
      show)
        case " $* " in
          *' LoadState '*) cat "$state/load" ;;
          *' FragmentPath '*) cat "$state/fragment" ;;
          *' ActiveState '*) cat "$state/active" ;;
          *) exit 1 ;;
        esac
        ;;
      enable) ;;
      restart)
        printf '%s\n' active > "$state/active"
        touch "$state/ready"
        ;;
      disable)
        printf '%s\n' inactive > "$state/active"
        ;;
      daemon-reload|status) ;;
      *) exit 1 ;;
    esac
    ;;
  milevox|milevox-home)
    [[ ${1:-} == status ]] || exit 1
    if [[ -f $state/manual || -f $state/ready || $(cat "$state/active") == active ]]; then
      if [[ -f $state/recording ]]; then printf '%s\n' '{"state":"recording"}';
      else printf '%s\n' '{"state":"idle"}'; fi
    else
      exit 1
    fi
    ;;
  milevox-download-model) touch "$state/model-ready" ;;
  *) exit 1 ;;
esac
EOF
  for command_name in milevox milevox-download-model systemctl; do
    ln -s -- fake-command "$BIN_DIR/$command_name"
  done
  cp -- "$BIN_DIR/fake-command" "$HOME_DIR/.local/bin/milevox"
}

run_setup() {
  env HOME="$HOME_DIR" MILEVOX_TEST_STATE="$STATE_DIR" \
    PATH="$BIN_DIR:/usr/bin:/bin" "$REPO_DIR/scripts/setup-user.sh"
}

run_teardown() {
  env HOME="$HOME_DIR" MILEVOX_TEST_STATE="$STATE_DIR" \
    PATH="$BIN_DIR:/usr/bin:/bin" "$REPO_DIR/scripts/teardown-user.sh"
}

run_raw_uninstall() {
  env HOME="$HOME_DIR" XDG_CONFIG_HOME="$HOME_DIR/.config" \
    MILEVOX_TEST_STATE="$STATE_DIR" PATH="$BIN_DIR:/usr/bin:/bin" \
    "$REPO_DIR/uninstall.sh"
}

test_setup() {
  local output
  new_home setup-manual
  touch "$STATE_DIR/manual"
  if run_setup >/dev/null 2>&1; then fail "setup accepted a manually running daemon"; fi
  ! grep -q '^systemctl --user restart' "$STATE_DIR/commands" || fail "manual-daemon setup restarted systemd"

  new_home setup-recording
  printf '%s\n' active > "$STATE_DIR/active"
  touch "$STATE_DIR/recording"
  output=$(run_setup 2>&1)
  [[ -f $STATE_DIR/model-ready && -f $STATE_DIR/ready ]] || fail "setup did not prepare model and service"
  grep -q '^systemctl --user enable milevox.service' "$STATE_DIR/commands" || fail "setup did not enable service"
  grep -q '^systemctl --user restart milevox.service' "$STATE_DIR/commands" || fail "setup did not restart service"
  [[ $output == *interrupted* ]] || fail "setup did not warn that recording was interrupted"
}

test_package_teardown() {
  new_home teardown-package
  printf '%s\n' loaded > "$STATE_DIR/load"
  printf '%s\n' /usr/lib/systemd/user/milevox.service > "$STATE_DIR/fragment"
  printf '%s\n' active > "$STATE_DIR/active"
  run_teardown >/dev/null
  grep -q '^systemctl --user disable --now milevox.service' "$STATE_DIR/commands" ||
    fail "teardown did not disable package service"

  new_home teardown-raw
  printf '%s\n' loaded > "$STATE_DIR/load"
  printf '%s\n' "$HOME_DIR/.config/systemd/user/milevox.service" > "$STATE_DIR/fragment"
  if run_teardown >/dev/null 2>&1; then fail "teardown managed a raw service"; fi
  ! grep -q '^systemctl --user disable' "$STATE_DIR/commands" || fail "teardown disabled a raw service"
}

test_raw_uninstall_ownership() {
  new_home raw-package-owned
  printf '%s\n' loaded > "$STATE_DIR/load"
  printf '%s\n' /usr/lib/systemd/user/milevox.service > "$STATE_DIR/fragment"
  if run_raw_uninstall >/dev/null 2>&1; then fail "raw uninstall managed package service"; fi
  [[ -x $HOME_DIR/.local/bin/milevox ]] || fail "raw uninstall removed binary on ownership failure"

  new_home raw-deleted-unit
  printf '%s\n' active > "$STATE_DIR/active"
  run_raw_uninstall >/dev/null
  grep -q '^systemctl --user disable --now milevox.service' "$STATE_DIR/commands" ||
    fail "raw uninstall did not stop a loaded service whose file was deleted"
  [[ ! -e $HOME_DIR/.local/bin/milevox ]] || fail "raw uninstall kept its binary"

  new_home raw-foreign
  printf '%s\n' loaded > "$STATE_DIR/load"
  printf '%s\n' /other/milevox.service > "$STATE_DIR/fragment"
  if run_raw_uninstall >/dev/null 2>&1; then fail "raw uninstall managed a foreign service"; fi
  ! grep -q '^systemctl --user disable' "$STATE_DIR/commands" || fail "raw uninstall stopped a foreign service"
}

main() {
  TEMP_DIR=$(mktemp -d)
  "$REPO_DIR/scripts/setup-user.sh" --help >/dev/null
  "$REPO_DIR/scripts/teardown-user.sh" --help >/dev/null
  "$REPO_DIR/uninstall.sh" --help >/dev/null
  test_setup
  test_package_teardown
  test_raw_uninstall_ownership
  printf '%s\n' 'Package lifecycle tests passed.'
}

trap cleanup EXIT
main "$@"
