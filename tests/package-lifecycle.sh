#!/usr/bin/env bash

# Verify package and raw per-user lifecycle ownership without a real systemd session.

set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_DIR
TEMP_DIR=""
HOME_DIR=""
CONFIG_HOME=""
STATE_DIR=""
BIN_DIR=""
RAW_INSTALL_DIR=""

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
  CONFIG_HOME=$HOME_DIR/.config
  STATE_DIR=$TEMP_DIR/$name/state
  BIN_DIR=$TEMP_DIR/$name/bin
  mkdir -p -- "$HOME_DIR/.local/bin" "$CONFIG_HOME/systemd/user" \
    "$STATE_DIR" "$BIN_DIR"
  printf '%s\n' not-found > "$STATE_DIR/load"
  : > "$STATE_DIR/fragment"
  printf '%s\n' inactive > "$STATE_DIR/active"
  printf '%s\n' idle > "$STATE_DIR/daemon-state"
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
    if [[ ${1:-} == --version ]]; then
      printf '%s\n' 'milevox 0.2.0'
      exit 0
    fi
    [[ ${1:-} == status ]] || exit 1
    if [[ -f $state/manual || -f $state/ready || $(cat "$state/active") == active ]]; then
      daemon_state=$(cat "$state/daemon-state")
      if [[ $daemon_state == error ]]; then
        printf '%s\n' '{"state":"error","notices":[{"level":"error","code":"model_unavailable","text":"Model unavailable"}]}'
      else
        printf '{"state":"%s"}\n' "$daemon_state"
      fi
    else
      exit 1
    fi
    ;;
  milevox-download-model) touch "$state/model-ready" ;;
  *) exit 1 ;;
esac
EOF
  for command_name in milevox milevox-download-model systemctl wl-copy wtype; do
    ln -s -- fake-command "$BIN_DIR/$command_name"
  done
  cp -- "$BIN_DIR/fake-command" "$HOME_DIR/.local/bin/milevox"
}

run_setup() {
  env HOME="$HOME_DIR" XDG_CONFIG_HOME="$CONFIG_HOME" \
    MILEVOX_TEST_STATE="$STATE_DIR" \
    PATH="$BIN_DIR:/usr/bin:/bin" "$REPO_DIR/scripts/setup-user.sh"
}

stage_raw_installer() {
  RAW_INSTALL_DIR=$TEMP_DIR/raw-installer
  mkdir -p -- "$RAW_INSTALL_DIR/bin" "$RAW_INSTALL_DIR/packaging/systemd"
  cp -- "$REPO_DIR/install.sh" "$RAW_INSTALL_DIR/install.sh"
  cp -- "$BIN_DIR/fake-command" "$RAW_INSTALL_DIR/bin/milevox"
  cp -- "$REPO_DIR/packaging/systemd/milevox.service" \
    "$REPO_DIR/packaging/systemd/environment" \
    "$RAW_INSTALL_DIR/packaging/systemd/"
}

run_raw_install() {
  env HOME="$HOME_DIR" XDG_CONFIG_HOME="$CONFIG_HOME" \
    MILEVOX_TEST_STATE="$STATE_DIR" PATH="$BIN_DIR:/usr/bin:/bin" \
    "$RAW_INSTALL_DIR/install.sh" --skip-model
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
  local before environment_file state
  new_home setup-manual
  touch "$STATE_DIR/manual"
  if run_setup >/dev/null 2>&1; then fail "setup accepted a manually running daemon"; fi
  ! grep -q '^systemctl --user restart' "$STATE_DIR/commands" || fail "manual-daemon setup restarted systemd"

  new_home setup-offline
  run_setup >/dev/null
  environment_file=$CONFIG_HOME/milevox/environment
  [[ -f $STATE_DIR/model-ready && -f $STATE_DIR/ready ]] ||
    fail "offline setup did not prepare the model and service"
  grep -q '^systemctl --user enable milevox.service' "$STATE_DIR/commands" || fail "setup did not enable service"
  grep -q '^systemctl --user restart milevox.service' "$STATE_DIR/commands" || fail "setup did not restart service"
  cmp -- "$REPO_DIR/packaging/systemd/environment" "$environment_file" ||
    fail "setup installed the wrong service environment example"
  [[ $(stat -c %a -- "$environment_file") == 600 ]] ||
    fail "setup created the service environment with the wrong mode"
  [[ $(stat -c %a -- "$CONFIG_HOME/milevox") == 700 ]] ||
    fail "setup created a non-private config directory"

  new_home setup-custom-config
  CONFIG_HOME=$TEMP_DIR/setup-custom-config/custom-config
  run_setup >/dev/null
  [[ -f $CONFIG_HOME/milevox/environment ]] ||
    fail "setup ignored XDG_CONFIG_HOME"

  new_home setup-existing-environment
  mkdir -p -- "$CONFIG_HOME/milevox"
  chmod 0755 "$CONFIG_HOME/milevox"
  environment_file=$CONFIG_HOME/milevox/environment
  printf '%s\n' 'OPENROUTER_API_KEY=greendale' > "$environment_file"
  chmod 0640 "$environment_file"
  before=$STATE_DIR/environment-before
  cp -p -- "$environment_file" "$before"
  run_setup >/dev/null
  cmp -- "$before" "$environment_file" ||
    fail "setup changed an existing service environment"
  [[ $(stat -c %a -- "$environment_file") == 640 ]] ||
    fail "setup changed the mode of an existing service environment"
  [[ $(stat -c %a -- "$CONFIG_HOME/milevox") == 700 ]] ||
    fail "setup did not secure an existing config directory"

  for state in recording transcribing refining canceling; do
    new_home "setup-busy-$state"
    printf '%s\n' active > "$STATE_DIR/active"
    printf '%s\n' "$state" > "$STATE_DIR/daemon-state"
    if run_setup >/dev/null 2>&1; then
      fail "setup restarted Milevox while $state"
    fi
    [[ ! -f $STATE_DIR/model-ready ]] ||
      fail "setup downloaded a model while $state"
    ! grep -q '^systemctl --user restart' "$STATE_DIR/commands" ||
      fail "setup restarted the service while $state"
  done

  new_home setup-model-unavailable
  printf '%s\n' error > "$STATE_DIR/daemon-state"
  if run_setup >/dev/null 2>&1; then
    fail "setup accepted a model_unavailable state"
  fi
}

test_raw_install_readiness_and_permissions() {
  local transition

  new_home raw-install-idle
  stage_raw_installer
  mkdir -p -- "$CONFIG_HOME/milevox"
  chmod 0755 "$CONFIG_HOME/milevox"
  run_raw_install >/dev/null
  cmp -- "$REPO_DIR/packaging/systemd/milevox.service" \
    "$CONFIG_HOME/systemd/user/milevox.service" ||
    fail "raw install changed the hardened service unit"
  [[ $(stat -c %a -- "$CONFIG_HOME/milevox") == 700 ]] ||
    fail "raw install did not secure the config directory"
  [[ $(stat -c %a -- "$CONFIG_HOME/milevox/environment") == 600 ]] ||
    fail "raw install created the service environment with the wrong mode"

  new_home raw-install-loading
  stage_raw_installer
  printf '%s\n' loading > "$STATE_DIR/daemon-state"
  (
    sleep 0.25
    printf '%s\n' idle > "$STATE_DIR/daemon-state"
  ) &
  transition=$!
  run_raw_install >/dev/null
  wait "$transition"
  [[ $(grep -c '^milevox status' "$STATE_DIR/commands") -gt 1 ]] ||
    fail "raw install accepted loading as ready"

  new_home raw-install-model-unavailable
  stage_raw_installer
  printf '%s\n' error > "$STATE_DIR/daemon-state"
  if run_raw_install >/dev/null 2>&1; then
    fail "raw install accepted a model_unavailable state"
  fi
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
  test_raw_install_readiness_and_permissions
  test_package_teardown
  test_raw_uninstall_ownership
  printf '%s\n' 'Package lifecycle tests passed.'
}

trap cleanup EXIT
main "$@"
