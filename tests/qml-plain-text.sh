#!/usr/bin/env bash

# Verify that daemon-controlled QML strings cannot enable rich-text rendering.

set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_DIR

fail() {
  printf 'QML plain-text test: %s\n' "$*" >&2
  exit 1
}

assert_plain_text_binding() {
  local file=$1 binding=$2
  local block

  block=$(grep -F -A 2 -- "$binding" "$file") ||
    fail "missing daemon binding '$binding' in ${file#"${REPO_DIR}/"}"
  grep -Fq -- 'textFormat: Text.PlainText' <<<"${block}" ||
    fail "daemon binding '$binding' can render rich text"
}

run_render_test() {
  local runner=/usr/lib/qt6/bin/qmltestrunner

  [[ -x ${runner} ]] || runner=$(command -v qmltestrunner || true)
  [[ -n ${runner} ]] || fail 'qmltestrunner is required'
  env -u DISPLAY -u WAYLAND_DISPLAY -u GDK_BACKEND -u QT_QPA_PLATFORMTHEME \
    QT_QPA_PLATFORM=minimal QT_QUICK_BACKEND=software \
    "${runner}" -input "${REPO_DIR}/tests/qml"
}

assert_single_status_process() {
  local count

  count=$(grep -RhoF -- '"status", "--follow", "--levels"' \
    "${REPO_DIR}/guis/omarchy" | wc -l)
  [[ ${count} == 1 ]] ||
    fail "expected one production status follower, found ${count}"
  grep -Fxq -- 'pragma Singleton' \
    "${REPO_DIR}/guis/omarchy/MilevoxStatus.qml" ||
    fail "MilevoxStatus is not declared as a singleton"
  grep -Fxq -- 'singleton MilevoxStatus 1.0 MilevoxStatus.qml' \
    "${REPO_DIR}/guis/omarchy/qmldir" ||
    fail "qmldir does not register the shared MilevoxStatus singleton"
  for file in Panel.qml MilevoxOverlay.qml; do
    grep -Fq -- 'readonly property var status: MilevoxStatus' \
      "${REPO_DIR}/guis/omarchy/${file}" ||
      fail "${file} does not use the shared MilevoxStatus singleton"
  done
}

main() {
  assert_plain_text_binding "${REPO_DIR}/guis/omarchy/Panel.qml" \
    'text: status.displayMessage'
  assert_plain_text_binding "${REPO_DIR}/guis/omarchy/Panel.qml" \
    'text: status.partialTranscript'
  assert_plain_text_binding "${REPO_DIR}/guis/omarchy/MilevoxOverlay.qml" \
    'text: root.displayText'
  assert_single_status_process
  run_render_test
}

main "$@"
