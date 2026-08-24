#!/usr/bin/env bash

# Shared release functions. This file is sourced by scripts and Make recipes.

normalize_architecture() {
  case "$1" in
    amd64|x86_64) printf '%s\n' x86_64 ;;
    arm64|aarch64) printf '%s\n' aarch64 ;;
    *) printf 'unsupported architecture: %s\n' "$1" >&2; return 1 ;;
  esac
}

validate_version() {
  [[ "$1" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]
}

project_version() {
  local root="$1" version
  version="$(awk -F '"' '/^version = / { print $2; exit }' "$root/Cargo.toml")"
  validate_version "$version" || { printf 'invalid project version: %s\n' "$version" >&2; return 1; }
  printf '%s\n' "$version"
}
