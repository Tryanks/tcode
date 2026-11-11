use gpui::{App, Context, EventEmitter, Task};
use std::{cell::RefCell, rc::Rc};
use tcode_client::speech::{Dictation, SpeechEvent, SpeechRecognizer, SpeechSession};
pub(super) use tcode_client::speech::{DictationEvent, SpeechStatus};

struct Session {
    platform: Rc<RefCell<Box<dyn SpeechSession>>>,
    _events: Task<()>,
}

pub(super) struct SpeechState {
    recognizer: Option<Rc<dyn SpeechRecognizer>>,
    locale: &'static str,
    dictation: Dictation,
    session: Option<Session>,
}

impl EventEmitter<DictationEvent> for SpeechState {}

impl SpeechState {
    pub(super) fn new(_: &mut Context<Self>) -> Self {
        Self {
            recognizer: None,
            locale: "en-US",
            dictation: Dictation::default(),
            session: None,
        }
    }

    pub(super) fn recognizer(mut self, recognizer: Rc<dyn SpeechRecognizer>) -> Self {
        self.recognizer = Some(recognizer);
        self
    }

    pub(super) fn has_recognizer(&self) -> bool {
        self.recognizer.is_some()
    }
    pub(super) fn is_available(&self, _: &App) -> bool {
        self.recognizer
            .as_ref()
            .is_some_and(|recognizer| recognizer.is_available(self.locale))
    }
    /// Re-reads what the platform answers availability from, then redraws.
    pub(super) fn refresh_permission(&self, cx: &mut Context<Self>) {
        let Some(refresh) = self
            .recognizer
            .as_ref()
            .and_then(|recognizer| recognizer.permission_refresh())
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move { refresh() })
                .await;
            _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }
    pub(super) fn status(&self) -> SpeechStatus {
        self.dictation.status()
    }
    pub(super) fn level(&self) -> f32 {
        self.dictation.level()
    }

    /// Start, stop, or, while a stop waits for the recognizer's final
    /// transcript, stop waiting.
    pub(super) fn toggle(&mut self, cx: &mut Context<Self>) {
        match self.status() {
            SpeechStatus::Stopping => self.cancel(cx),
            status if status.is_active() => self.stop(cx),
            _ => self.start(cx),
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(recognizer) = self.recognizer.clone() else {
            return;
        };
        let id = self.dictation.start();
        let (sender, receiver) = async_channel::unbounded();
        let platform = match recognizer.start(self.locale, sender) {
            Ok(platform) => Rc::new(RefCell::new(platform)),
            Err(error) => {
                self.on_events(id, [SpeechEvent::Error(error)], cx);
                return;
            }
        };
        let events = cx.spawn({
            let platform = platform.clone();
            async move |this, cx| {
                futures_lite::future::or(
                    async {
                        while let Ok(event) = receiver.recv().await {
                            // gpui-base's text wrapper drops its width cache on
                            // every edit and re-shapes the whole edited line, so
                            // each write costs as much as the dictated line is
                            // long. What queued while the main thread was busy,
                            // as when speech stops, is applied as one revision.
                            let mut events = vec![event];
                            while let Ok(event) = receiver.try_recv() {
                                events.push(event);
                            }
                            let active = this.update(cx, |this, cx| {
                                this.on_events(id, events, cx);
                                this.dictation.is_session(id)
                            });
                            if !matches!(active, Ok(true)) {
                                break;
                            }
                        }
                    },
                    async {
                        std::future::poll_fn(|cx| platform.borrow_mut().poll(cx)).await;
                        std::future::pending().await
                    },
                )
                .await;
            }
        });
        self.session = Some(Session {
            platform,
            _events: events,
        });
        cx.emit(DictationEvent::Started);
        cx.notify();
    }

    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        if !self.dictation.stop() {
            return;
        }
        // The final transcript is the recognizer's: the session ends when
        // the platform says it finished or failed, and the user can stop
        // waiting for it ([`Self::cancel`]).
        if let Some(session) = &self.session {
            session.platform.borrow_mut().finish();
        }
        cx.notify();
    }

    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(event) = self.dictation.cancel() {
            self.session = None;
            cx.emit(event);
            cx.notify();
        }
    }

    fn on_events(
        &mut self,
        id: u64,
        events: impl IntoIterator<Item = SpeechEvent>,
        cx: &mut Context<Self>,
    ) {
        for event in self.dictation.apply(id, events) {
            cx.emit(event);
        }
        if !self.dictation.status().is_active() {
            self.session = None;
        }
        cx.notify();
    }
}

pub(super) fn configure(mut state: SpeechState, locale: &'static str, cx: &App) -> SpeechState {
    state.locale = locale;
    let recognizer = cx
        .global::<crate::remote::ClientAttachment>()
        .host()
        .speech_recognizer();
    if let Some(recognizer) = recognizer {
        return state.recognizer(recognizer);
    }
    state.recognizer = None;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if state.recognizer.is_none() {
        state = state.recognizer(Rc::new(tcode_client::speech::SystemRecognizer::new(locale)));
    }
    state
}
