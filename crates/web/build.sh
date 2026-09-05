#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DIST_DIR="${SCRIPT_DIR}/dist"
cd "${REPO_ROOT}"
if [[ "$(wasm-bindgen --version)" != "wasm-bindgen 0.2.121" ]]; then
  echo 'Install matching CLI: cargo install wasm-bindgen-cli --version 0.2.121 --locked' >&2
  exit 1
fi
cargo build -p tcode-web --target wasm32-unknown-unknown --release
mkdir -p "${DIST_DIR}"
wasm-bindgen --target web --out-dir "${DIST_DIR}" --out-name tcode_web \
  "${CARGO_TARGET_DIR:-${REPO_ROOT}/target}/wasm32-unknown-unknown/release/tcode_web.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
    "${DIST_DIR}/tcode_web_bg.wasm" -o "${DIST_DIR}/tcode_web_bg.optimized.wasm"
  mv "${DIST_DIR}/tcode_web_bg.optimized.wasm" "${DIST_DIR}/tcode_web_bg.wasm"
else
  echo 'Notice: wasm-opt unavailable; keeping the unoptimized wasm-bindgen output.' >&2
fi
cp "${SCRIPT_DIR}/static/index.html" "${DIST_DIR}/index.html"
printf 'Built browser bundle: %s\n' "${DIST_DIR}"
