#!/usr/bin/env bash
set -euo pipefail
fail() { echo "milevox-teardown: $*" >&2; exit 1; }
usage() { echo 'Usage: milevox-teardown'; }
main() {
  [[ ${1:-} != -h && ${1:-} != --help ]] || { usage; return; }
  (( $# == 0 )) || fail "unexpected argument: $1"
  (( EUID != 0 )) || fail "run this command without sudo"
  command -v systemctl >/dev/null || fail "systemctl is required"
  local fragment load
  load="$(systemctl --user show milevox.service -p LoadState --value 2>/dev/null || true)"
  fragment="$(systemctl --user show milevox.service -p FragmentPath --value 2>/dev/null || true)"
  if [[ "$load" == loaded && "$fragment" == /usr/lib/systemd/user/milevox.service ]]; then
    systemctl --user disable --now milevox.service
  elif [[ "$load" == loaded ]]; then
    fail "refusing to manage non-package service: $fragment"
  fi
  echo 'Milevox service disabled. Configuration, models, and other user data were preserved.'
}
main "$@"
