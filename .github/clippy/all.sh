#!/bin/sh
# Clippy for the three platforms CI runs it on, from one machine: this one
# natively, Windows by cross-checking the MSVC target (cargo-xwin), Linux in
# a container built from ./Dockerfile. Code under `cfg(windows)` or
# `cfg(target_os = "linux")` is otherwise first compiled by CI.
#
#   .github/clippy/all.sh            every platform this machine can do
#   .github/clippy/all.sh windows    one of: host, windows, linux
#
# Needs: for windows, `cargo install cargo-xwin` and llvm-rc (LLVM_RC names
# it when it is not on PATH); for linux, Docker.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$repo" || exit 1
failed=

host() {
  cargo clippy --workspace --all-targets --locked -- -D warnings
}

windows() {
  command -v cargo-xwin >/dev/null || { echo "windows: cargo-xwin is not installed" >&2; return 2; }
  if ! command -v "${LLVM_RC:-llvm-rc}" >/dev/null; then
    for candidate in /opt/homebrew/opt/llvm/bin/llvm-rc /usr/local/opt/llvm/bin/llvm-rc; do
      [ -x "$candidate" ] && LLVM_RC="$candidate" && export LLVM_RC && break
    done
  fi
  command -v "${LLVM_RC:-llvm-rc}" >/dev/null || { echo "windows: llvm-rc was not found" >&2; return 2; }
  PATH="$here:$PATH" CARGO_PROFILE_DEV_DEBUG=0 \
    cargo xwin clippy --workspace --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
}

linux() {
  command -v docker >/dev/null || { echo "linux: docker is not installed" >&2; return 2; }
  docker build -q -t tcode-linux-clippy "$here" >/dev/null || return 1
  docker run --rm -v "$repo":/src:ro \
    -v tcode-linux-clippy-target:/target -v tcode-linux-clippy-registry:/opt/cargo/registry \
    tcode-linux-clippy cargo clippy --workspace --all-targets --locked -- -D warnings
}

for platform in ${*:-host windows linux}; do
  echo "== clippy: $platform"
  case "$platform" in
    host|windows|linux) "$platform" || failed="$failed $platform" ;;
    *) echo "unknown platform: $platform" >&2; exit 2 ;;
  esac
done
[ -z "$failed" ] || { echo "clippy failed or could not run for:$failed" >&2; exit 1; }
