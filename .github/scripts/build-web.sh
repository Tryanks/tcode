#!/usr/bin/env bash
set -euo pipefail

cargo build -p tcode-web --target wasm32-unknown-unknown --release --locked
mkdir -p crates/web/dist
wasm-bindgen --target web --out-dir crates/web/dist --out-name tcode_web \
    target/wasm32-unknown-unknown/release/tcode_web.wasm
wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
    --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue \
    crates/web/dist/tcode_web_bg.wasm -o crates/web/dist/tcode_web_bg.optimized.wasm
mv crates/web/dist/tcode_web_bg.optimized.wasm crates/web/dist/tcode_web_bg.wasm
cp crates/web/static/index.html crates/web/static/auth.mjs crates/web/dist/
ls -lh crates/web/dist
