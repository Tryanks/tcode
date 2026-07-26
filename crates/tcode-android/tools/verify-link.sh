#!/usr/bin/env bash
# Prove the Android shared library links.
#
# The counterpart to crates/tcode-ios/tools/verify-link.sh. A cdylib is linked
# by construction, so unlike iOS this needs no separate harness — but it does
# need the NDK, and the toolchain variables are easy to get wrong. Capturing the
# invocation means the next person does not rediscover them.
#
# `cargo check` is not enough on its own: it never runs the linker, so it cannot
# tell you whether the symbols gpui_wgpu, wgpu and psm need from the NDK's libc
# resolve.
set -euo pipefail

cd "$(dirname "$0")/../../.."
NDK=${ANDROID_NDK_HOME:-/opt/homebrew/share/android-ndk}
API=${ANDROID_API_LEVEL:-26}
PROFILE=${1:-debug}

[ -d "$NDK" ] || { echo "no NDK at $NDK; set ANDROID_NDK_HOME"; exit 1; }

# The prebuilt directory is darwin-x86_64 even on Apple silicon.
BIN=$(echo "$NDK"/toolchains/llvm/prebuilt/*/bin)
export ANDROID_NDK_HOME="$NDK"
export CC_aarch64_linux_android="$BIN/aarch64-linux-android$API-clang"
export AR_aarch64_linux_android="$BIN/llvm-ar"

ARGS=(-t arm64-v8a --platform "$API" build -p tcode-android)
[ "$PROFILE" = release ] && ARGS+=(--release)
cargo ndk "${ARGS[@]}" 2>&1 | tail -1

SO="target/aarch64-linux-android/$PROFILE/libtcode_android.so"
[ -f "$SO" ] || { echo "no shared object at $SO"; exit 1; }

file "$SO"
echo "JNI entry points exported:"
"$BIN/llvm-nm" -D --defined-only "$SO" \
  | grep -oE "Java_com_tryanks_tcode_TcodeActivity_native[A-Za-z]+" \
  | sort -u | sed 's/^/  /'
