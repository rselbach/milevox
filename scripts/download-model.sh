#!/usr/bin/env bash

# Download the Parakeet TDT model files used by Milevox.

set -euo pipefail

readonly MODEL_NAME="parakeet-tdt-0.6b-v2-int8"
readonly MODEL_URL="https://huggingface.co/istupakov/\
parakeet-tdt-0.6b-v2-onnx/resolve/main"
readonly ENCODER_SHA256="3e0581fda6ab843888b51e56d7ee78b6d5bc3237ec113af1f732d1d5286aa155"
readonly DECODER_SHA256="a449f49acd68979d418651dd2dcb737cc0f1bf0225e009e29ee326354edbf7d3"
readonly VOCAB_SHA256="ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d"

fail() {
  echo "download-model: $*" >&2
  exit 1
}

file_has_checksum() {
  local path="$1"
  local want="$2"
  local checksum

  [[ -f "${path}" ]] || return 1
  checksum="$(sha256sum -- "${path}")"
  checksum="${checksum%% *}"
  [[ "${checksum}" == "${want}" ]]
}

download_file() {
  local model_dir="$1"
  local filename="$2"
  local checksum="$3"
  local destination="${model_dir}/${filename}"
  local partial="${destination}.part"

  if file_has_checksum "${destination}" "${checksum}"; then
    echo "Model file already installed: ${destination}"
    return
  fi

  if file_has_checksum "${partial}" "${checksum}"; then
    mv -- "${partial}" "${destination}"
    echo "Model file already downloaded: ${destination}"
    return
  fi

  rm -f -- "${partial}"

  echo "Downloading ${filename}"
  curl \
    --continue-at - \
    --fail \
    --location \
    --retry 3 \
    --output "${partial}" \
    "${MODEL_URL}/${filename}"

  file_has_checksum "${partial}" "${checksum}" ||
    fail "downloaded ${filename} failed its SHA-256 check"
  mv -- "${partial}" "${destination}"
}

main() {
  local data_home
  local model_dir

  command -v curl >/dev/null || fail "curl is required"
  command -v sha256sum >/dev/null || fail "sha256sum is required"

  data_home="${XDG_DATA_HOME:-${HOME}/.local/share}"
  model_dir="${1:-${data_home}/milevox/models/${MODEL_NAME}}"
  mkdir -p -- "${model_dir}"

  download_file \
    "${model_dir}" "encoder-model.int8.onnx" "${ENCODER_SHA256}"
  download_file \
    "${model_dir}" "decoder_joint-model.int8.onnx" "${DECODER_SHA256}"
  download_file "${model_dir}" "vocab.txt" "${VOCAB_SHA256}"

  echo "Parakeet model ready: ${model_dir}"
}

main "$@"
