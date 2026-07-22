#!/usr/bin/env bash
set -euo pipefail

VERSION="1.19.28"
ASSET="mihomo-linux-amd64-v2-v${VERSION}"
ARCHIVE="${ASSET}.gz"
SHA256="b94cb855da57ee666c77fd48c42bd94f5e091510c3c950659a9a853a1673a6e3"
URL="https://github.com/MetaCubeX/mihomo/releases/download/v${VERSION}/${ARCHIVE}"
CACHE_ROOT="${XDG_CACHE_HOME:-${HOME}/.cache}/rsclash/mihomo-tests/v${VERSION}"
BINARY="${CACHE_ROOT}/${ASSET}"

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  echo "The pinned integration runner currently supports Linux x86_64 only." >&2
  exit 2
fi

mkdir -p "${CACHE_ROOT}"
chmod 700 "${CACHE_ROOT}"

if [[ ! -f "${CACHE_ROOT}/${ARCHIVE}" ]]; then
  curl -fL "${URL}" -o "${CACHE_ROOT}/${ARCHIVE}.part"
  mv "${CACHE_ROOT}/${ARCHIVE}.part" "${CACHE_ROOT}/${ARCHIVE}"
fi

actual_sha256="$(sha256sum "${CACHE_ROOT}/${ARCHIVE}" | cut -d ' ' -f 1)"
if [[ "${actual_sha256}" != "${SHA256}" ]]; then
  echo "Mihomo archive checksum mismatch." >&2
  exit 1
fi

if [[ ! -x "${BINARY}" ]]; then
  gzip -dc "${CACHE_ROOT}/${ARCHIVE}" > "${BINARY}.part"
  chmod 700 "${BINARY}.part"
  mv "${BINARY}.part" "${BINARY}"
fi

RSCLASH_MIHOMO_BIN="${BINARY}" \
  cargo test -p rsclash-mihomo --test real_mihomo -- --ignored --nocapture
RSCLASH_MIHOMO_BIN="${BINARY}" \
  cargo test -p rsclash-config --test real_mihomo -- --ignored --nocapture
