# Android APK size investigation

Baseline: `6e5d8d921`, measured 2026-09-08 with NDK 27.1.12297006 and
`crates/android/host/build.sh --release`. All values below are bytes; MB means
1,000,000 bytes and MiB means 1,048,576 bytes. Measurements use `unzip -l`, ZIP
entry compressed sizes, NDK `llvm-size -A` and `llvm-objdump -h`.

The baseline APK is 92,062,015 bytes (87.80 MiB). Its main library is 298,474,816
bytes, including 258,410,891 bytes of DWARF. Optimization level 3 and fat LTO
were already enabled; `debug = 1` still produces enormous merged debug data.
`llvm-strip --strip-debug` alone produces 39,339,504 bytes;
`llvm-strip --strip-unneeded` reduces that same library to 30,345,104 bytes
(13,371,795 bytes with DEFLATE level 6), keeping JNI exports and unwind tables.

## APK entries

Final level-3 APK (including intervening upstream preview/navigation changes), rebased onto `d2f25a251`: **38,023,742 bytes (36.26 MiB)**,
a **58.70%** reduction. All entry sizes below come from the measured ZIP.

| Entry | Before uncompressed | Before in ZIP | Final uncompressed | Final in ZIP |
| --- | ---: | ---: | ---: | ---: |
| `classes.dex` | 11,640,012 | 4,425,829 | 1,626,708 | 712,228 |
| `classes2.dex` | 136,756 | 41,165 | 0 | 0 |
| `classes3.dex` | 25,732 | 12,846 | 0 | 0 |
| `classes4.dex` | 318,748 | 133,896 | 0 | 0 |
| `lib/arm64-v8a/libbarhopper_v3.so` | 4,946,720 | 2,236,121 | 4,946,720 | 4,946,720 |
| `lib/arm64-v8a/libimage_processing_util_jni.so` | 29,008 | 15,377 | 29,008 | 29,008 |
| `lib/arm64-v8a/libsurface_util_jni.so` | 4,832 | 1,840 | 4,832 | 4,832 |
| `lib/arm64-v8a/libtcode_android.so` | 298,474,816 | 73,622,548 | 30,963,984 | 30,963,984 |
| `assets/fonts/NotoColorEmoji.ttf` | 10,673,480 | 9,956,367 | 0 | 0 |
| `assets/fonts/OFL-NotoColorEmoji.txt` | 4,301 | 1,986 | 0 | 0 |
| `assets/mlkit_barcode_models/barcode_ssd_mobilenet_v1_dmp25_quant.tflite` | 390,456 | 390,456 | 390,456 | 390,456 |
| `assets/mlkit_barcode_models/oned_auto_regressor_mobile.tflite` | 213,880 | 213,880 | 213,880 | 213,880 |
| `assets/mlkit_barcode_models/oned_feature_extractor_mobile.tflite` | 276,552 | 276,552 | 276,552 | 276,552 |
| `res/` | 318,890 | 221,377 | 197,072 | 132,370 |
| `other` | 451,838 | 423,431 | 308,362 | 280,060 |
| `assets/dexopt/baseline.prof` | 0 | 0 | 1,178 | 1,178 |
| `assets/dexopt/baseline.profm` | 0 | 0 | 189 | 189 |

ZIP directory, alignment and signatures also contribute to APK file length.
The primary post-change native library is stored rather than compressed.
The four stored native entries total 35,944,544 bytes; separately compressing
those same bytes with DEFLATE-6 gives 15,774,965 bytes. Thus modern packaging
adds about 20.17 MB of native ZIP payload versus that compression, before
alignment/signatures. This allows direct loading from the APK, avoiding a separate installed native
copy. It trades a larger download for smaller installed storage. The final ZIP has 37,951,457 bytes of live payload and 72,285 bytes of
directory/signature/alignment overhead. `zipalign -c -P 16 -v 4` passes, and the
merged manifest sets `extractNativeLibs=false`. The APK avoids extracting another
35,944,544 bytes of native libraries (excluding ART/cache/data storage). R8 reduces
DEX from 12,121,248 to 1,626,708 bytes and removes unused resources; the QR scanner
retains its ML Kit native library and three bundled offline model files.

## Native sections

| Library / variant | .text | .rodata | .debug_* total |
| --- | ---: | ---: | ---: |
| before: `libbarhopper_v3.so` | 3,932,216 | 204,584 | 0 |
| before: `libimage_processing_util_jni.so` | 19,332 | 263 | 0 |
| before: `libsurface_util_jni.so` | 280 | 0 | 0 |
| before: `libtcode_android.so` | 20,085,744 | 4,366,388 | 258,410,891 |
| after-3: `libtcode_android.so` | 20,098,780 | 4,366,828 | 0 |
| after-s: `libtcode_android.so` | 16,047,616 | 4,419,500 | 0 |
| after-z: `libtcode_android.so` | 11,911,172 | 4,455,540 | 0 |

The other three native libraries have unchanged sections after packaging.
Final level-3 sections after rebase: `.text` 20,378,156; `.rodata` 4,384,236;
`.debug_*` 0; `.eh_frame` 1,956,924; `.gcc_except_table` 1,573,240.

Baseline DWARF: `.debug_str` 128,019,375; `.debug_info` 68,283,320;
`.debug_ranges` 36,367,536; `.debug_line` 24,424,865; `.debug_aranges` 900,112;
`.debug_loc` 313,630; `.debug_abbrev` 102,053. Other retained sections include
exception/unwind tables, relocations, and read-only relocated data; removing
those would break unwinding or loading.

## Desktop comparison

`cargo build --release -p tcode --locked`, followed by `strip` on a copy, produces
a 37,953,184-byte macOS arm64 binary. Its DEFLATE-6 payload is 17,594,695 bytes.
The stripped Android level-3 library is 30,362,256 bytes, with a DEFLATE-6 payload
of 13,382,582 bytes. After rebasing, the final Android library is 30,963,984 bytes, or 13,652,787
with DEFLATE-6. The APK stores that library uncompressed. Thus Android's
application native binary is actually smaller than the desktop binary; comparing
the desktop's roughly 20 MB archive to the whole APK had mixed compression,
debug-symbol and packaging overheads.

## Asset audit

- Android's old `assets/fonts` held only Noto Color Emoji and its license, not
  DM Sans or Lilex. Removing them saves 10,677,781 raw / 9,958,353 ZIP bytes.
- DM Sans is 240,164 bytes; Lilex's four styles total 783,048 bytes. They are each
  included once through `crates/ui/src/assets.rs` and registered by `run_shell`.
  Exact byte-sequence searches of the stripped library confirm one copy of each
  font. They remain to preserve shared UI typography and readable code blocks.
- `gpui-kit-assets 0.6.0` embeds only `icons/**/*.svg`: 101 files / 48,132 bytes.
  It does not include other image formats, fonts or themes. Removing arbitrary
  icons risks controls constructed inside the component library for little gain.
- The application assets owner layers a few SVG icons over that source; it is
  not a broad rust-embed of the entire repository. The theme is a small JSON input.
- The 990,780-byte syntax database is used by Android's shared code rendering.
  The web WASM/JS bundle is embedded only by the headless binary's `web` feature,
  outside Android's dependency graph. No desktop/WASM blob was found to trim.

Five pre-API-26 launcher PNGs were removed: minSdk 26 always selects the adaptive
vector icon. The syntax database also occurs exactly once in the binary.

## Packaging and symbols

The script retains the exact unstripped Rust library under
`crates/android/host/app/build/unstripped/arm64-v8a/`, and strips only the copy
passed to Gradle. Preserve that directory and its matching APK, plus R8's
`app/build/outputs/mapping/release/mapping.txt`. The [backend README](../crates/platform/gpui-android/README.md)
contains the `ndk-stack` command.

`--release` now assembles Gradle Release, with R8 and resource shrinking and a
local debug signature. JNI-only `gpui*` methods, `previewHost`, and
`PreviewHost.command` are kept explicitly, including the preview feature added
on the final rebase. The no-flag build remains Gradle Debug. Both retain only
arm64-v8a. The script also removes the staged `jniLibs/arm64-v8a/libtcode_android.so`
before `cargo ndk`: its timestamp-only freshness check otherwise reused the newer
debug artifact during a cached release build. The regression reproduced an
81,032,864-byte stripped **debug** library in an 88,092,294-byte "release" APK;
this was live code rather than ZIP dead space. Removing staging forces the correct
profile's artifact to be copied, including when re-running an already stripped release.

Fresh `outputs` and `intermediates` prevent AGP's incremental ZIP writer
from retaining dead space when the native library shrinks. The maintainer also
observed a 188 MB APK with only 34 MB of live compressed entries on `cf004bcf1`;
that separate artifact is not the clean baseline measured above. The final
no-flag APK measured 766,784,551 bytes; switching immediately to release
now returns to 38,023,742 bytes and preserves the full 327,284,152-byte symbol
file. This verifies both the profile-switch and ZIP-dead-space regressions.

Symbolization was exercised with a synthetic frame at the exported `android_main`
address: `ndk-stack` resolved it to `crates/android/src/lib.rs:8:0`. Exact `.text`,
`.rodata`, and `.eh_frame` bytes match between the packaged and unstripped copies;
the JNI exports and `ANativeActivity_onCreate` remain in the dynamic symbol table.

## System emoji

The tested API-35 image's system font is only 2,779,576 bytes and uses COLRv1.
The old backend explicitly skipped it because Swash cannot rasterize COLRv1.
The Android text-system adapter keeps cosmic-text shaping, then uses Android
`Font.getGlyphBounds` / software `Canvas.drawGlyphs` for color rasterization on
API 31+. Older bitmap fonts retain the Swash path. This removes the need for a
second font while preserving Chinese and color emoji, including composer input.

A missing system font is unrealistic on supported stock devices (minSdk 26).
A custom-ROM distribution can optionally provide the old bitmap asset and its
license; it is read only if `/system/fonts/NotoColorEmoji.ttf` is absent. Standard
builds no longer contain that optional asset.

References: [Android Font API](https://developer.android.com/reference/android/graphics/fonts/Font),
[Android 13 COLRv1 support](https://developer.android.com/about/versions/13/features),
[Android 12 compatibility definition](https://source.android.com/docs/compatibility/12/android-12-cdd.pdf).

## Optimization experiments and final validation

Profiles were tested on the same pre-rebase code and Java/resources. Each used
fat LTO, one codegen unit and unwind panics. `3` remains the shipping default.

| opt-level | APK bytes | Stripped .so bytes | DEFLATE-6 .so bytes | Median cold-process launch | Median presented-frame interval | p95 interval |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 37,463,222 | 30,362,256 | 13,382,582 | 739 ms | 34.09 ms | 75.59 ms |
| s | 34,906,582 | 27,805,616 | 11,139,244 | 610 ms | 31.19 ms | 46.87 ms |
| z | 32,591,686 | 25,490,720 | 11,445,923 | 294 ms | 34.14 ms | 82.64 ms |

Relative to level 3, `s` saved 6.82% APK bytes, with observed median launch -17.5%
and frame interval -8.5%; `z` saved 13.00%, with observed launch -60.2%, median
frame interval +0.2% and p95 interval +9.3%. These are rough emulator observations,
not proven optimization speedups: five force-stop/start samples after one discarded
warm-up, then six alternating 1.5-second swipes over 100 synthetic Chinese/emoji
thread rows. SurfaceFlinger supplied 233/321/212 inter-frame intervals respectively.
`gfxinfo` reports zero frames for this native Vulkan surface and was not used.

The emulator used the exact `tcode-p2d` API-35 arm64 Pixel-6 configuration
(1080×2400, 420 dpi, 4 virtual CPUs, 2 GB RAM, software Vulkan). Other tasks
changed the shared emulator mid-run, so those trials were discarded; the reported
set used a distinctly named temporary copy, `apk-size-private` on port 5672, with
its own userdata and the same system image/configuration. Filesystem caches were
warm; host scheduling and software Vulkan still introduce substantial noise.

Level 3 stays enabled because this small sample does not establish performance
for markdown, syntax highlighting or large conversation replay on a phone, and
`z` worsened tail frame pacing in the measured list. `s`/`z` remain concrete size
options for a separate physical-device workload decision, rather than silently
changing general code optimization on this evidence alone.

The required gates passed on the final base: `cargo fmt --all --check`,
`CARGO_NDK_PLATFORM=26 RUSTFLAGS='-D warnings' cargo ndk -t arm64-v8a check -p tcode-android --locked`,
`cargo test -p tcode-ui --locked` (356 unit tests and one integration test), and
both `crates/android/host/build.sh` and `crates/android/host/build.sh --release`,
with the SDK/NDK environment given above. Shell syntax and `git diff --check`
also pass. One intermediate test run failed the existing temporary-file test
`markdown::link_target::tests::strips_line_and_column_suffixes_for_existing_paths`;
the full final-tree rerun passed without changing that unrelated test.

Evidence is retained locally under `/tmp/tcode-apk-size/`: `before/` and `final/`
contain `unzip-l.txt`, `sizes.json`, and per-library LLVM section listings;
`delivery-*.log` contain validation output. `private/benchmark-{3,s,z}.json`
contain the controlled timing samples. Screenshots are external evidence, not
application assets: `delivery-start.png` shows Machines on `tcode-p2d`, and
`final-composer-light.png` shows the final APK with Chinese UI and an unsent
color emoji in the composer on the isolated matching emulator configuration.
`final-composer-dark.png` records the same font adapter in dark appearance before
the final upstream navigation-only rebase.
The release APK also opens the Android preview surface after R8, exercising the
kept JNI bridge. No provider message was sent during validation.

The stock API-35 system font was exercised; an API-26–30 device and a custom ROM
without the system font were not available. Their bitmap fallback path remains,
but is not claimed as device-tested. Physical-device performance remains the
limitation of the optimization comparison.
