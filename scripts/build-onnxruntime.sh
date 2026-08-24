#!/usr/bin/env bash

# Build the ONNX Runtime revision matched by ort 2.0.0-rc.13 on the host ABI.

set -euo pipefail

readonly ONNXRUNTIME_REVISION="da9b5e364c465de65c49d91e696cd6485270757f"

fail() {
  printf 'build-onnxruntime: %s\n' "$*" >&2
  exit 1
}

session_contains_model_package() {
  local session_archive="$1"

  [[ -f $session_archive ]] &&
    nm -g --defined-only "$session_archive" |
      grep -F ' T ModelPackage_Open' >/dev/null
}

merge_model_package() {
  local release_dir="$1"
  local session_archive="$release_dir/libonnxruntime_session.a"
  local model_package_archive="$release_dir/model_package/libmodel_package.a"
  local merge_dir merged_archive

  [[ -f $session_archive && -f $model_package_archive ]] ||
    fail "ONNX Runtime produced no model-package archive"
  session_contains_model_package "$session_archive" && return 0

  merge_dir=$(mktemp -d)
  merged_archive="$release_dir/.libonnxruntime_session.merged.a"
  if ! cp -- "$session_archive" "$merged_archive" ||
    ! (cd -- "$merge_dir" && ar --extract "$model_package_archive") ||
    ! ar rcs "$merged_archive" "$merge_dir"/*.o ||
    ! mv -f -- "$merged_archive" "$session_archive"; then
    rm -rf -- "$merge_dir" "$merged_archive"
    fail "could not add ONNX Runtime's model-package objects to its session archive"
  fi
  rm -rf -- "$merge_dir"
}

main() {
  local destination="${1:-}" source_dir build_dir marker command_name cmake_version
  [[ $# -eq 1 && -n $destination ]] || fail "usage: $0 <build-directory>"
  for command_name in ar cmake git nm python3; do
    command -v "$command_name" >/dev/null || fail "$command_name is required"
  done
  cmake_version=$(cmake --version | awk 'NR == 1 { print $3 }')
  [[ $(printf '%s\n%s\n' "$cmake_version" 3.28 | sort -V | head -n 1) == 3.28 ]] ||
    fail "CMake 3.28 or newer is required (found $cmake_version)"

  source_dir=$destination/source
  build_dir=$destination/build
  marker=$destination/revision
  if [[ -f $marker && $(cat -- "$marker") == "$ONNXRUNTIME_REVISION" &&
    -f $build_dir/Release/libonnxruntime_common.a &&
    -f $build_dir/Release/_deps/re2-build/libre2.a ]] &&
    session_contains_model_package "$build_dir/Release/libonnxruntime_session.a"; then
    printf '%s\n' "$build_dir/Release"
    return 0
  fi
  [[ ! -e $destination ]] || fail "$destination exists without a completed matching build"
  mkdir -p -- "$source_dir"

  git -C "$source_dir" init --quiet
  git -C "$source_dir" remote add origin https://github.com/microsoft/onnxruntime.git
  git -C "$source_dir" fetch --quiet --depth 1 origin "$ONNXRUNTIME_REVISION"
  git -C "$source_dir" checkout --quiet --detach FETCH_HEAD
  git -C "$source_dir" submodule update --init --recursive --depth 1
  "$source_dir/build.sh" --build_dir "$build_dir" --config Release \
    --update --build --parallel --skip_tests --compile_no_warning_as_error \
    --cmake_extra_defines onnxruntime_BUILD_UNIT_TESTS=OFF
  # ONNX Runtime's default static build does not make its RE2 archive part of
  # the all target, but ort-sys links that archive as a separate prerequisite.
  cmake --build "$build_dir/Release" --config Release --target re2 --parallel
  # ort-sys rc.13 predates ONNX Runtime's standalone model-package archive, so
  # include those objects in the session component that ort-sys already links.
  merge_model_package "$build_dir/Release"
  if [[ ! -f $build_dir/Release/libonnxruntime_common.a ||
    ! -f $build_dir/Release/_deps/re2-build/libre2.a ]] ||
    ! session_contains_model_package "$build_dir/Release/libonnxruntime_session.a"; then
    fail "ONNX Runtime produced an incomplete static library tree"
  fi
  printf '%s\n' "$ONNXRUNTIME_REVISION" > "$marker"
  printf '%s\n' "$build_dir/Release"
}

main "$@"
