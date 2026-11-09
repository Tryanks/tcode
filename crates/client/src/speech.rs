//! Client-local speech capture. Transcripts never pass through the remote host.

#[derive(Debug, Clone, PartialEq)]
pub enum SpeechEvent {
    Ready,
    Level(f32),
    Hypothesis(String),
    Phrase(String),
    Finished,
    PermissionDenied,
    Unsupported,
    Error(SpeechError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechError {
    PermissionDenied,
    NoInputDevice,
    Unsupported,
    Input(String),
    Recognizer(String),
}

impl std::fmt::Display for SpeechError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionDenied => f.write_str("microphone access was denied"),
            Self::NoInputDevice => f.write_str("no audio input device is available"),
            Self::Unsupported => f.write_str("speech input is not supported"),
            Self::Input(error) => write!(f, "audio input failed: {error}"),
            Self::Recognizer(error) => write!(f, "speech recognition failed: {error}"),
        }
    }
}
impl std::error::Error for SpeechError {}
impl From<String> for SpeechError {
    fn from(error: String) -> Self {
        Self::Recognizer(error)
    }
}
impl From<&str> for SpeechError {
    fn from(error: &str) -> Self {
        Self::Recognizer(error.into())
    }
}

/// The platform owns microphone capture, permissions and recognition resources.
/// Dropping a session cancels it; finish flushes its last result before Finished.
pub trait SpeechSession {
    fn finish(&mut self);

    /// Drive platform callbacks on the client thread. Callback-only platforms
    /// can leave this pending; dropping the session cancels this work too.
    fn poll(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        std::task::Poll::Pending
    }
}

pub trait SpeechRecognizer {
    /// Answers from what the platform already knows: every render asks.
    fn is_available(&self, locale: &str) -> bool;
    /// Work that re-reads a permission the user can change outside Tcode, as
    /// in System Settings, for `is_available` to answer from afterwards. It
    /// may block on a system service, so it runs off the main thread.
    fn permission_refresh(&self) -> Option<fn()> {
        None
    }
    fn start(
        &self,
        locale: &str,
        events: async_channel::Sender<SpeechEvent>,
    ) -> Result<Box<dyn SpeechSession>, SpeechError>;
}

#[cfg(all(feature = "voice", any(target_os = "macos", target_os = "windows")))]
mod level;
#[cfg(all(feature = "voice", target_os = "macos"))]
mod macos;
#[cfg(all(feature = "voice", target_os = "windows"))]
mod windows;
#[cfg(all(feature = "voice", target_os = "macos"))]
pub use macos::SystemRecognizer;
#[cfg(all(feature = "voice", target_os = "windows"))]
pub use windows::SystemRecognizer;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SpeechStatus {
    #[default]
    Idle,
    Connecting,
    Recording,
    Stopping,
}

impl SpeechStatus {
    pub fn is_active(self) -> bool {
        self != Self::Idle
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DictationEvent {
    Started,
    Revised(Revision),
    /// The recognizer is done: the last revision's hypothesis is final.
    Final,
    Cancelled,
    Error(SpeechError),
}

/// What the recognizer changed since the previous revision: `settled`
/// replaces the hypothesis the previous revision left, and `hypothesis`
/// follows it until a later revision replaces it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Revision {
    pub settled: String,
    pub hypothesis: String,
}

/// One client-local dictation. Each session has a distinct identity so queued
/// platform callbacks cannot write into a replacement session.
#[derive(Default)]
pub struct Dictation {
    next_session: u64,
    session: Option<u64>,
    status: SpeechStatus,
    level: f32,
}

impl Dictation {
    pub fn start(&mut self) -> u64 {
        self.next_session += 1;
        self.session = Some(self.next_session);
        self.status = SpeechStatus::Connecting;
        self.level = 0.;
        self.next_session
    }

    pub fn status(&self) -> SpeechStatus {
        self.status
    }
    pub fn level(&self) -> f32 {
        self.level
    }
    pub fn is_session(&self, id: u64) -> bool {
        self.session == Some(id)
    }

    pub fn stop(&mut self) -> bool {
        if matches!(
            self.status,
            SpeechStatus::Connecting | SpeechStatus::Recording
        ) {
            self.status = SpeechStatus::Stopping;
            true
        } else {
            false
        }
    }

    pub fn cancel(&mut self) -> Option<DictationEvent> {
        self.session?;
        self.end();
        Some(DictationEvent::Cancelled)
    }

    /// Applies platform events that arrived together, in order. Their text
    /// becomes one revision, ahead of the event that ends the session, so
    /// a backlog costs the draft one edit however long it grew.
    pub fn apply(
        &mut self,
        id: u64,
        events: impl IntoIterator<Item = SpeechEvent>,
    ) -> Vec<DictationEvent> {
        let mut revision = None::<Revision>;
        let mut applied = Vec::new();
        for event in events {
            if !self.is_session(id) {
                break;
            }
            let end = match event {
                SpeechEvent::Ready => {
                    if self.status == SpeechStatus::Connecting {
                        self.status = SpeechStatus::Recording;
                    }
                    continue;
                }
                SpeechEvent::Level(level) => {
                    self.level = level;
                    continue;
                }
                SpeechEvent::Hypothesis(text) => {
                    revision.get_or_insert_default().hypothesis = text;
                    continue;
                }
                SpeechEvent::Phrase(text) => {
                    let revision = revision.get_or_insert_default();
                    revision.settled.push_str(&text);
                    revision.hypothesis.clear();
                    continue;
                }
                SpeechEvent::Finished => DictationEvent::Final,
                SpeechEvent::PermissionDenied => {
                    DictationEvent::Error(SpeechError::PermissionDenied)
                }
                SpeechEvent::Unsupported => DictationEvent::Error(SpeechError::Unsupported),
                SpeechEvent::Error(error) => DictationEvent::Error(error),
            };
            self.end();
            applied.extend(revision.take().map(DictationEvent::Revised));
            applied.push(end);
        }
        applied.extend(revision.map(DictationEvent::Revised));
        applied
    }

    fn end(&mut self) {
        self.session = None;
        self.status = SpeechStatus::Idle;
        self.level = 0.;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revised(settled: &str, hypothesis: &str) -> DictationEvent {
        DictationEvent::Revised(Revision {
            settled: settled.into(),
            hypothesis: hypothesis.into(),
        })
    }

    #[test]
    fn revisions_coalesce_queued_results_and_terminal_events_are_session_local() {
        let mut state = Dictation::default();
        let id = state.start();
        assert_eq!(state.apply(id, [SpeechEvent::Ready]), []);
        assert_eq!(state.status(), SpeechStatus::Recording);
        for (event, revision) in [
            (
                SpeechEvent::Hypothesis("hello word".into()),
                revised("", "hello word"),
            ),
            (
                SpeechEvent::Phrase("Hello world.".into()),
                revised("Hello world.", ""),
            ),
        ] {
            assert_eq!(state.apply(id, [event]), [revision]);
        }
        // Results that queued together are one revision: every settled
        // phrase in order, then the latest hypothesis.
        assert_eq!(
            state.apply(
                id,
                [
                    SpeechEvent::Hypothesis(" How".into()),
                    SpeechEvent::Phrase(" How are you?".into()),
                    SpeechEvent::Level(0.5),
                    SpeechEvent::Hypothesis(" I am".into()),
                    SpeechEvent::Hypothesis(" I'm fine".into()),
                ]
            ),
            [revised(" How are you?", " I'm fine")]
        );
        assert!(state.stop());
        state.apply(id, [SpeechEvent::Ready]);
        assert_eq!(state.status(), SpeechStatus::Stopping);
        assert_eq!(
            state.apply(
                id,
                [
                    SpeechEvent::Phrase(" I'm fine.".into()),
                    SpeechEvent::Finished,
                    SpeechEvent::Hypothesis("late".into()),
                ]
            ),
            [revised(" I'm fine.", ""), DictationEvent::Final]
        );
        assert_eq!(state.status(), SpeechStatus::Idle);
        assert_eq!(
            state.apply(id, [SpeechEvent::Hypothesis("late".into())]),
            []
        );

        let cancelled = state.start();
        state.apply(cancelled, [SpeechEvent::Hypothesis("discard".into())]);
        assert_eq!(state.cancel(), Some(DictationEvent::Cancelled));
        for terminal in [
            SpeechEvent::PermissionDenied,
            SpeechEvent::Unsupported,
            SpeechEvent::Error("offline".into()),
        ] {
            let current = state.start();
            assert_eq!(
                state.apply(cancelled, [SpeechEvent::Phrase("late".into())]),
                []
            );
            assert!(matches!(
                state.apply(current, [terminal]).as_slice(),
                [DictationEvent::Error(_)]
            ));
            assert_eq!(state.status(), SpeechStatus::Idle);
            assert_eq!(state.apply(current, [SpeechEvent::Finished]), []);
        }
    }
}
