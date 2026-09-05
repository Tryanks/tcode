#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

export ANDROID_HOME="${ANDROID_HOME:-/opt/homebrew/share/android-commandlinetools}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/27.1.12297006}"
export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21}"
export CARGO_NDK_PLATFORM="${CARGO_NDK_PLATFORM:-26}"

cd "$CRATE_DIR"
# rust-embed normally reads assets from the current directory in dev builds,
# which does not exist inside an APK. Embed only the existing asset crate (and
# its rust-embed runtime) while keeping debug assertions for application code.
cargo ndk -t arm64-v8a -o host/app/src/main/jniLibs build -p tcode-android \
    --config 'profile.dev.package.gpui-kit-assets.debug-assertions=false' \
    --config 'profile.dev.package.rust-embed.debug-assertions=false'

cd "$SCRIPT_DIR"
./gradlew assembleDebug
