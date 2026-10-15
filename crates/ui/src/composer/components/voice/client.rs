use gpui::{App, Subscription, Task};
use gpui_component::speech::{
    AudioFormat, AudioInput, AudioSink, RecognitionSession, SpeechError, SpeechRecognizer,
    SpeechSink, SpeechState,
};
use std::rc::Rc;
use tcode_client::speech::{SpeechEvent, SpeechSession};

struct ClientRecognizer {
    platform: Rc<dyn tcode_client::speech::SpeechRecognizer>,
    locale: &'static str,
}

struct Session {
    platform: Box<dyn SpeechSession>,
    _events: Task<()>,
}

impl SpeechRecognizer for ClientRecognizer {
    fn is_available(&self, _: &App) -> bool {
        self.platform.is_available(self.locale)
    }

    fn start(
        &self,
        sink: SpeechSink,
        cx: &mut App,
    ) -> Result<Box<dyn RecognitionSession>, SpeechError> {
        let (sender, receiver) = async_channel::unbounded();
        let platform = self
            .platform
            .start(self.locale, sender)
            .map_err(|error| SpeechError::recognizer(std::io::Error::other(error)))?;
        let events = cx.spawn(async move |cx| {
            while let Ok(event) = receiver.recv().await {
                let terminal = matches!(
                    event,
                    SpeechEvent::Finished
                        | SpeechEvent::Error(_)
                        | SpeechEvent::PermissionDenied
                        | SpeechEvent::Unsupported
                );
                cx.update(|cx| match event {
                    SpeechEvent::Ready => sink.ready(cx),
                    SpeechEvent::Hypothesis(text) => sink.hypothesis(text, cx),
                    SpeechEvent::Phrase(text) => sink.phrase(text, cx),
                    SpeechEvent::Finished => sink.finish(cx),
                    SpeechEvent::PermissionDenied => sink.error(SpeechError::PermissionDenied, cx),
                    SpeechEvent::Unsupported => sink.error(SpeechError::Unsupported, cx),
                    SpeechEvent::Error(error) => {
                        sink.error(SpeechError::recognizer(std::io::Error::other(error)), cx)
                    }
                });
                if terminal {
                    break;
                }
            }
        });
        Ok(Box::new(Session {
            platform,
            _events: events,
        }))
    }
}

impl RecognitionSession for Session {
    fn push_audio(&mut self, _: &[i16], _: &mut App) {}
    fn finish(&mut self, _: &mut App) {
        self.platform.finish();
    }
}

struct PlatformCapture;
impl AudioInput for PlatformCapture {
    fn start(
        &self,
        _: AudioFormat,
        _: AudioSink,
        _: &mut App,
    ) -> Result<Subscription, SpeechError> {
        // The ClientHost session owns capture and releases it on finish or drop.
        Ok(Subscription::new(|| {}))
    }
}

pub(super) fn configure(state: SpeechState, locale: &'static str, cx: &App) -> SpeechState {
    if let Some(platform) = cx
        .try_global::<crate::remote::ClientAttachment>()
        .and_then(|attachment| attachment.host().speech_recognizer())
    {
        return state
            .recognizer(ClientRecognizer { platform, locale })
            .input(PlatformCapture);
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let state = state.recognizer(gpui_component::speech::SystemRecognizer::new().locale(locale));
    state
}
