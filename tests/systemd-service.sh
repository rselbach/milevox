#!/usr/bin/env bash

# Verify service hardening and both installed unit variants.

set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_DIR
TEMP_DIR=""

fail() {
  echo "systemd service test: $*" >&2
  exit 1
}

cleanup() {
  [[ -z ${TEMP_DIR} ]] || rm -rf -- "${TEMP_DIR}"
}

check_hardening() {
  local unit="$1"
  local directive
  local directives=(
    NoNewPrivileges=true
    UMask=0077
    LimitCORE=0
    PrivateTmp=true
    ProtectSystem=strict
    ProtectKernelTunables=true
    ProtectKernelModules=true
    ProtectKernelLogs=true
    ProtectControlGroups=true
    RestrictSUIDSGID=true
    LockPersonality=true
    'RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6'
  )

  for directive in "${directives[@]}"; do
    grep -Fx -- "${directive}" "${unit}" >/dev/null ||
      fail "missing directive in ${unit}: ${directive}"
  done
  ! grep -Eq '^(PrivateNetwork|ProtectHome)=' "${unit}" ||
    fail "unit contains an unapproved network or home restriction"
}

smoke_hardening() {
  local cargo
  local coverage_entry
  local coverage_tests
  local file
  local run_command
  local test_name
  local unit
  local properties=(
    --property=NoNewPrivileges=true
    --property=UMask=0077
    --property=LimitCORE=0
    --property=PrivateTmp=true
    --property=ProtectSystem=strict
    --property=ProtectKernelTunables=true
    --property=ProtectKernelModules=true
    --property=ProtectKernelLogs=true
    --property=ProtectControlGroups=true
    --property=RestrictSUIDSGID=true
    --property=LockPersonality=true
    '--property=RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6'
  )

  case "${MILEVOX_SYSTEMD_SCOPE:-user}" in
    system)
      command -v sudo >/dev/null ||
        fail "sudo is required for the system-manager hardening smoke test"
      run_command=(
        sudo --non-interactive systemd-run
        --uid="$(id -u)"
        --gid="$(id -g)"
      )
      ;;
    user)
      if ! command -v systemd-run >/dev/null ||
        ! systemctl --user show-environment >/dev/null 2>&1; then
        [[ ${MILEVOX_REQUIRE_SYSTEMD_SMOKE:-0} != 1 ]] ||
          fail "a systemd user manager is required for the hardening smoke test"
        echo "Skipping systemd behavior smoke: no user manager is available."
        return
      fi
      run_command=(systemd-run --user)
      ;;
    *)
      fail "unknown systemd smoke scope: ${MILEVOX_SYSTEMD_SCOPE}"
      ;;
  esac
  command -v cargo >/dev/null || fail "cargo is required for the systemd smoke test"
  cargo="$(command -v cargo)"
  unit="milevox-hardening-test-$$"

  coverage_tests=(
    'src/transcription.rs|supervised_worker_loads_quickly_and_stays_resident_for_requests'
    'src/output.rs|wtype_receives_text_only_on_stdin_with_a_reviewed_environment'
    'src/output.rs|wl_copy_reports_stderr_and_receives_stdin'
    'src/credentials.rs|saves_credentials_with_user_only_permissions'
    'src/daemon.rs|debug_entries_append_across_sessions_and_correct_permissions'
    'src/post_processing.rs|post_processor_reuses_one_http_connection'
  )
  for coverage_entry in "${coverage_tests[@]}"; do
    file="${coverage_entry%%|*}"
    test_name="${coverage_entry#*|}"
    grep -F -- "fn ${test_name}" "${REPO_DIR}/${file}" >/dev/null ||
      fail "systemd app smoke lost required coverage: ${test_name}"
  done

  # Run the real module paths, including the fake local model and loopback cloud
  # fixtures, under the same restrictions as the installed service.
  "${run_command[@]}" --wait --collect --pipe --quiet --unit="${unit}" \
    --working-directory="${REPO_DIR}" \
    "${properties[@]}" \
    --setenv="PATH=$(dirname -- "${cargo}"):/usr/local/bin:/usr/bin:/bin" \
    "${cargo}" test --locked --all-targets
}

main() {
  local source_unit
  local raw_unit
  local package_unit
  local verify_unit

  source_unit="${REPO_DIR}/packaging/systemd/milevox.service"
  TEMP_DIR="$(mktemp -d)"
  trap cleanup EXIT
  raw_unit="${TEMP_DIR}/raw/milevox.service"
  package_unit="${TEMP_DIR}/package/milevox.service"
  verify_unit="${TEMP_DIR}/verify/milevox.service"

  install -Dm0644 "${source_unit}" "${raw_unit}"
  install -Dm0644 "${source_unit}" "${package_unit}"
  sed -i 's|%h/.local/bin/milevox|/usr/bin/milevox|' "${package_unit}"

  grep -Fx 'ExecStart=%h/.local/bin/milevox daemon' "${raw_unit}" >/dev/null ||
    fail "raw installer unit has the wrong ExecStart"
  grep -Fx 'ExecStart=/usr/bin/milevox daemon' "${package_unit}" >/dev/null ||
    fail "pacman unit has the wrong ExecStart"
  check_hardening "${raw_unit}"
  check_hardening "${package_unit}"
  if [[ -n ${MILEVOX_PACKAGE_UNIT:-} ]]; then
    [[ -f ${MILEVOX_PACKAGE_UNIT} ]] ||
      fail "built package unit not found: ${MILEVOX_PACKAGE_UNIT}"
    grep -Fx 'ExecStart=/usr/bin/milevox daemon' \
      "${MILEVOX_PACKAGE_UNIT}" >/dev/null ||
      fail "the built package unit has the wrong ExecStart"
    check_hardening "${MILEVOX_PACKAGE_UNIT}"
  fi

  if [[ ${MILEVOX_VERIFY_SYSTEMD:-0} == 1 ]]; then
    command -v systemd-analyze >/dev/null ||
      fail "systemd-analyze is required"
    install -Dm0644 "${source_unit}" "${verify_unit}"
    sed -i 's|^ExecStart=.*|ExecStart=/bin/true|' "${verify_unit}"
    systemd-analyze verify "${verify_unit}"
    smoke_hardening
  fi

  echo "Systemd service tests passed."
}

main "$@"
