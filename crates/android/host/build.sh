#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

export ANDROID_HOME="${ANDROID_HOME:-/opt/homebrew/share/android-commandlinetools}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/27.1.12297006}"
export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21}"
export CARGO_NDK_PLATFORM="${CARGO_NDK_PLATFORM:-26}"

cd "$CRATE_DIR"
cargo ndk -t arm64-v8a -o host/app/src/main/jniLibs build -p tcode-android

cd "$SCRIPT_DIR"
./gradlew assembleDebug
