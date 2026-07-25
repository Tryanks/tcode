#!/usr/bin/env bash
# Prove the iOS staticlib links.
#
# `cargo build` for a staticlib only archives object files — it never runs the
# linker, so it cannot tell you whether every symbol gpui, gpui_wgpu, Metal and
# cosmic-text need actually resolves. This links the archive into a real
# executable against the iOS SDK, which does.
#
# The Android side gets this for free: a cdylib is linked by construction. iOS
# needs it done deliberately, which is why this script exists.
set -euo pipefail

cd "$(dirname "$0")/../../.."
TARGET=aarch64-apple-ios
PROFILE=${1:-release}
DEPLOY=${IOS_DEPLOYMENT_TARGET:-17.0}

cargo build -p tcode-ios --target "$TARGET" --profile "$PROFILE" 2>&1 | tail -1
ARCHIVE="target/$TARGET/$PROFILE/libtcode_ios.a"
[ -f "$ARCHIVE" ] || { echo "no archive at $ARCHIVE"; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Every entry point is referenced so the linker must resolve the whole graph
# rather than proving most of it dead.
cat > "$WORK/main.m" <<'OBJC'
#import <Foundation/Foundation.h>
extern void tcode_ios_start(void *, uint32_t, uint32_t, float,
                            void (*)(void *), void (*)(void *),
                            void (*)(void *), void (*)(void *), void *);
extern void tcode_ios_surface_created(void *, uint32_t, uint32_t, float);
extern void tcode_ios_surface_destroyed(void);
extern void tcode_ios_resized(uint32_t, uint32_t, float);
extern void tcode_ios_frame(void);
extern void tcode_ios_drain_main_thread(void);
extern void tcode_ios_touch(uint64_t, int32_t, float, float, float);
extern void tcode_ios_lifecycle(int32_t);
extern void tcode_ios_init_logging(const char *);
static void noop(void *ctx) { (void)ctx; }
int main(int argc, char **argv) {
    if (argc > 99) {
        tcode_ios_start(NULL, 0, 0, 0.0f, noop, noop, noop, noop, NULL);
        tcode_ios_surface_created(NULL, 0, 0, 0.0f);
        tcode_ios_surface_destroyed();
        tcode_ios_resized(0, 0, 0.0f);
        tcode_ios_frame();
        tcode_ios_drain_main_thread();
        tcode_ios_touch(0, 0, 0, 0, 0);
        tcode_ios_lifecycle(0);
        tcode_ios_init_logging("tcode");
    }
    return 0;
}
OBJC

SDK=$(xcrun --sdk iphoneos --show-sdk-path)
xcrun --sdk iphoneos clang \
  -target "arm64-apple-ios$DEPLOY" -isysroot "$SDK" \
  "$WORK/main.m" "$ARCHIVE" \
  -framework Foundation -framework UIKit -framework Metal \
  -framework QuartzCore -framework CoreGraphics -framework CoreText \
  -framework Security \
  -o "$WORK/probe"

file "$WORK/probe"
echo "entry points linked:"
nm -gU "$WORK/probe" | grep -oE "_tcode_ios_[a-z_]+" | sort -u | sed 's/^/  /'
