//! Live dictation backed by the macOS 26 SpeechAnalyzer/SpeechTranscriber API.
//!
//! On macOS 26+ (built against an SDK ≥ 26) [`is_supported`] returns true and
//! [`start`] opens a microphone dictation session whose transcript is streamed
//! through [`DictationEvent`]s. Everywhere else the crate compiles to a stub
//! that reports unsupported.
//!
//! Event contract: after `start` returns, the callback receives at most one
//! `Ready`, then any number of `Volatile`/`Final` events, then exactly one
//! terminal event (`Error` or `Ended`). Each `Volatile` replaces the previous
//! volatile text; a `Final` commits text (the current volatile range becomes
//! the final text and a new empty volatile range begins after it). Callbacks
//! arrive on arbitrary non-UI threads.

/// A transcription event delivered to the callback passed to [`start`].
#[derive(Debug, Clone)]
pub enum DictationEvent {
    /// Speech assets are installed and the microphone is live.
    Ready,
    /// Microphone input level in `0.0..=1.0`, reported continuously while
    /// recording (roughly at the audio callback rate). Purely cosmetic.
    Level(f32),
    /// In-progress hypothesis; replaces the previous `Volatile` text.
    Volatile(String),
    /// Finalized text; replaces the current volatile text and is stable.
    Final(String),
    /// The session failed. Terminal.
    Error(String),
    /// The session ended after [`DictationSession::stop`] or drop. Terminal.
    Ended,
}

/// A running dictation session. Dropping it cancels recognition.
pub struct DictationSession {
    #[allow(dead_code)]
    imp: imp::Session,
}

impl DictationSession {
    /// Stop capturing and finalize: remaining audio is decoded, a last
    /// `Final` may be delivered, then `Ended`.
    pub fn stop(&self) {
        self.imp.stop();
    }
}

/// Whether live dictation is available on this machine (macOS 26+, shim
/// compiled in, and the locale returned by [`preferred_locale`] supported).
pub fn is_supported() -> bool {
    imp::is_supported()
}

/// Map an app locale (`"zh-CN"`, `"en"`, …) to the dictation locale to use.
pub fn preferred_locale(app_locale: &str) -> &'static str {
    if app_locale.starts_with("zh") { "zh_CN" } else { "en_US" }
}

/// Start a microphone dictation session for `locale` (an identifier such as
/// `"zh_CN"`). Events are delivered on arbitrary threads.
pub fn start(
    locale: &str,
    on_event: Box<dyn Fn(DictationEvent) + Send + Sync>,
) -> Result<DictationSession, String> {
    imp::start(locale, on_event).map(|imp| DictationSession { imp })
}

/// Start transcription from an audio file.
///
/// This is an implementation hook for examples and integration testing.
#[doc(hidden)]
pub fn start_file(
    locale: &str,
    path: &str,
    on_event: Box<dyn Fn(DictationEvent) + Send + Sync>,
) -> Result<DictationSession, String> {
    imp::start_file(locale, path, on_event).map(|imp| DictationSession { imp })
}

#[cfg(all(target_os = "macos", voice_shim))]
mod imp;

#[cfg(not(all(target_os = "macos", voice_shim)))]
mod imp {
    use super::DictationEvent;

    pub struct Session;

    impl Session {
        pub fn stop(&self) {}
    }

    pub fn is_supported() -> bool {
        false
    }

    pub fn start(
        _locale: &str,
        _on_event: Box<dyn Fn(DictationEvent) + Send + Sync>,
    ) -> Result<Session, String> {
        Err("dictation is not supported on this platform".into())
    }

    pub fn start_file(
        _locale: &str,
        _path: &str,
        _on_event: Box<dyn Fn(DictationEvent) + Send + Sync>,
    ) -> Result<Session, String> {
        Err("dictation is not supported on this platform".into())
    }
}
