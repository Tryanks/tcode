//! Client-local speech capture. Transcripts never pass through the remote host.

pub enum SpeechEvent {
    Ready,
    Hypothesis(String),
    Phrase(String),
    Finished,
    PermissionDenied,
    Unsupported,
    Error(String),
}

/// The platform owns microphone capture, permissions and recognition resources.
/// Dropping a session cancels it; finish flushes its last result before Finished.
pub trait SpeechSession {
    fn finish(&mut self);
}

pub trait SpeechRecognizer {
    fn is_available(&self, locale: &str) -> bool;
    fn start(
        &self,
        locale: &str,
        events: async_channel::Sender<SpeechEvent>,
    ) -> Result<Box<dyn SpeechSession>, String>;
}
