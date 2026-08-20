# Voice input (macOS 26 live dictation) — implementation plan

Branch: `feat/voice-input`. Research: `docs/voice-input-research.md`.

Scope: live transcription into the composer via the macOS 26
SpeechAnalyzer/SpeechTranscriber API. The mic button exists **only** on
macOS 26+; on every other platform/version it is absent. No settings
section, no cloud backends, no voice mode.

## Architecture

- `crates/voice` (`tcode-voice`): Swift shim (`swift/shim.swift`, C ABI via
  `@_cdecl`) compiled by `build.rs` with `swiftc` when the SDK is ≥ 26,
  gated by the `voice_shim` cfg; stub elsewhere. Swift owns the entire audio
  path (AVAudioEngine mic → SpeechAnalyzer). Public API pinned in
  `crates/voice/src/lib.rs`: `is_supported()`, `preferred_locale()`,
  `start(locale, callback) -> DictationSession`, events
  `Ready | Volatile | Final | Error | Ended`.
- `crates/ui`: mic toggle button in the composer control row
  (macOS + `is_supported()` only). Insertion anchor at cursor; each
  `Volatile` replaces the provisional range via
  `set_selected_range` + `TextareaState::replace`; `Final` commits and moves
  the anchor. Esc or a second click stops; submit/thread-switch stops too.
- Release packaging: `NSMicrophoneUsageDescription` (+
  `NSSpeechRecognitionUsageDescription`) added to the generated Info.plist
  in `.github/workflows/release.yml`.

## Acceptance criteria

1. `cargo run -p tcode-voice --example file_dictation -- /tmp/voice-smoke/zh.aiff zh_CN`
   prints ≥ 3 `volatile:` lines and one `final:` line containing 超时处理,
   then `ended`. (Audio fixture from `say -v Tingting`; regenerate with
   `say -v Tingting -o /tmp/voice-smoke/zh.aiff "把这个函数改成异步的，然后加一个超时处理"`.)
2. `cargo check --workspace` and `cargo build` pass on macOS.
3. With `TCODE_VOICE_FORCE_STUB=1` (build.rs env override), the workspace
   still builds and `is_supported()` is false — proves the non-mac path.
4. In the running app: mic button visible in the composer, click starts
   (button shows active state), speech appears live in the editor, second
   click stops and keeps the text. Esc during recording stops it.
5. `cargo test -p tcode-ui` passes; locale key-set test passes (en + zh-CN).
