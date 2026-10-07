//! System dictation and replacement of the session's whole transcript.

use super::super::*;
use gpui::SharedString;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use gpui_component::speech::SystemRecognizer;
use gpui_component::speech::{SpeechEvent, SpeechState, SpeechStatus};

pub(in super::super) struct Voice {
    state: Entity<SpeechState>,
    locale: &'static str,
    insertion: Option<Insertion>,
}

struct Insertion {
    anchor: usize,
    last_len: usize,
    expected: String,
}

fn speech_locale() -> &'static str {
    if rust_i18n::locale().starts_with("zh") {
        "zh-CN"
    } else {
        "en-US"
    }
}

fn speech_state(cx: &mut Context<SpeechState>) -> SpeechState {
    let state = SpeechState::new(cx);
    // On Windows the system recognizer uses Microsoft's online speech service.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let state = state.recognizer(SystemRecognizer::new().locale(speech_locale()));
    state
}

impl Voice {
    pub(in super::super) fn new(
        window: &mut Window,
        cx: &mut Context<Composer>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let state = cx.new(speech_state);
        subscriptions.push(
            cx.subscribe_in(&state, window, |this, _, event, window, cx| {
                this.on_dictation_event(event, window, cx);
            }),
        );
        subscriptions.push(cx.observe(&state, |_, _, cx| cx.notify()));
        subscriptions.push(cx.on_release(|this, cx| {
            this.voice.insertion = None;
            this.voice.state.update(cx, |state, cx| state.cancel(cx));
        }));
        Self {
            state,
            locale: speech_locale(),
            insertion: None,
        }
    }
}

impl Composer {
    pub(in super::super) fn render_mic_button(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let locale = speech_locale();
        if locale != self.voice.locale {
            self.abort_dictation(cx);
            self.voice.state.update(cx, |state, cx| {
                // Preserve session ids so a previous sink cannot address a new session.
                let previous = std::mem::replace(state, SpeechState::new(cx));
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                let previous = previous.recognizer(SystemRecognizer::new().locale(locale));
                *state = previous;
            });
            self.voice.locale = locale;
        }
        let state = self.voice.state.read(cx);
        if !state.has_recognizer() {
            return None;
        }
        let status = state.status();
        let preparing = matches!(status, SpeechStatus::Connecting | SpeechStatus::Stopping);
        let available = state.is_available(cx);
        let tooltip: SharedString = if !available {
            crate::tr!("composer.voice_unavailable")
        } else if preparing {
            crate::tr!("composer.voice_preparing")
        } else if status.is_active() {
            crate::tr!("composer.voice_stop")
        } else {
            crate::tr!("composer.voice_start")
        }
        .into_owned()
        .into();
        let color = if status == SpeechStatus::Recording {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };
        // Kit SpeechButton/SpeechWaveform require gpui-component's theme global, which Tcode does not initialise.
        Some(
            Button::new("voice-mic")
                .ghost()
                .compact()
                .h(px(28.))
                .rounded(crate::material::radius_chip(cx))
                .disabled(!available)
                .tooltip(tooltip)
                .child(if preparing {
                    Spinner::new().small().color(color).into_any_element()
                } else {
                    let icon = Icon::empty()
                        .path("icons/mic.svg")
                        .small()
                        .text_color(color);
                    if status == SpeechStatus::Recording {
                        div()
                            .rounded_full()
                            .p(px(2.))
                            .bg(cx
                                .theme()
                                .danger
                                .opacity(0.08 + state.levels().last().unwrap_or(0.) * 0.42))
                            .child(icon)
                            .into_any_element()
                    } else {
                        icon.into_any_element()
                    }
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.voice.state.update(cx, |state, cx| state.toggle(cx));
                }))
                .into_any_element(),
        )
    }

    pub(in super::super) fn stop_dictation(&mut self, cx: &mut Context<Self>) -> bool {
        let active = self.voice.state.read(cx).status().is_active();
        if active {
            self.voice.state.update(cx, |state, cx| state.stop(cx));
        }
        active
    }

    pub(in super::super) fn abort_dictation(&mut self, cx: &mut Context<Self>) {
        self.voice.insertion = None;
        self.voice.state.update(cx, |state, cx| state.cancel(cx));
    }

    pub(in super::super) fn stop_dictation_on_user_edit(&mut self, cx: &mut Context<Self>) {
        if self.voice.state.read(cx).status().is_active()
            && self
                .voice
                .insertion
                .as_ref()
                .is_none_or(|insertion| insertion.expected != self.input.read(cx).value().as_ref())
        {
            self.abort_dictation(cx);
        }
    }

    fn on_dictation_event(
        &mut self,
        event: &SpeechEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SpeechEvent::Started => {
                // A cancelled session can have a Started event already queued.
                if self.voice.state.read(cx).status().is_active() {
                    let (anchor, expected) = self.input.update(cx, |input, cx| {
                        input.focus(window, cx);
                        (input.cursor(), input.value().to_string())
                    });
                    self.voice.insertion = Some(Insertion {
                        anchor,
                        last_len: 0,
                        expected,
                    });
                }
            }
            SpeechEvent::Partial(text) | SpeechEvent::Final(text) => {
                self.insert_transcript(text.clone(), window, cx);
                if matches!(event, SpeechEvent::Final(_)) {
                    self.voice.insertion = None;
                }
            }
            SpeechEvent::Error(error) => {
                self.voice.insertion = None;
                window.push_notification(
                    Notification::error(
                        crate::tr!("composer.voice_error", error = error).into_owned(),
                    ),
                    cx,
                );
            }
            SpeechEvent::Cancelled => self.voice.insertion = None,
        }
        cx.notify();
    }

    fn insert_transcript(
        &mut self,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(insertion) = self.voice.insertion.as_mut() else {
            return;
        };
        if self.input.read(cx).value().as_ref() != insertion.expected {
            self.abort_dictation(cx);
            return;
        }
        let range = insertion.anchor..insertion.anchor + insertion.last_len;
        self.input.update(cx, |input, cx| {
            input.set_selected_range(range, cx);
            input.replace(text.clone(), window, cx);
        });
        insertion.last_len = text.len();
        // Change events run after the update; our writes must match before delivery.
        insertion.expected = self.input.read(cx).value().to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};
    use gpui_component::speech::{
        AudioFormat, AudioInput, AudioSink, RecognitionSession, SpeechError, SpeechRecognizer,
        SpeechSink,
    };
    use std::cell::RefCell;

    struct Recognizer(Rc<RefCell<Option<SpeechSink>>>);
    struct Recognition;

    impl SpeechRecognizer for Recognizer {
        fn start(
            &self,
            sink: SpeechSink,
            cx: &mut App,
        ) -> Result<Box<dyn RecognitionSession>, SpeechError> {
            sink.ready(cx);
            *self.0.borrow_mut() = Some(sink);
            Ok(Box::new(Recognition))
        }
    }

    impl RecognitionSession for Recognition {
        fn push_audio(&mut self, _: &[i16], _: &mut App) {}
        fn finish(&mut self, _: &mut App) {}
    }

    struct SilentInput;
    impl AudioInput for SilentInput {
        fn start(
            &self,
            _: AudioFormat,
            _: AudioSink,
            _: &mut App,
        ) -> Result<Subscription, SpeechError> {
            Ok(Subscription::new(|| {}))
        }
    }

    #[gpui::test]
    fn dictation_revises_the_draft_and_cannot_overwrite_edits_or_submission(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(std::env::temp_dir().join(format!(
                "tcode-dictation-test-{}-{}",
                std::process::id(),
                tcode_services::store::now_millis()
            )))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("dictation".into(), std::env::temp_dir(), cx);
            let timeline = state.residents.live[&id].timeline.clone();
            (id, timeline)
        }))
        .unwrap();
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let sink = Rc::new(RefCell::new(None::<SpeechSink>));
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let composer = Composer::new(store.clone(), window, cx);
            composer.voice.state.update(cx, |state, cx| {
                *state = SpeechState::new(cx)
                    .recognizer(Recognizer(sink.clone()))
                    .input(SilentInput);
            });
            composer
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        composer.update_in(cx, |composer, window, cx| {
            composer.set_draft("前后", window, cx);
            composer.input.update(cx, |input, cx| {
                input.set_selected_range(3..3, cx);
            });
        });
        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let first = sink.borrow().clone().unwrap();
        for (text, expected) in [
            ("hello brave world", "前hello brave world后"),
            ("hi", "前hi后"),
            ("", "前后"),
            ("你好", "前你好后"),
        ] {
            cx.update(|_, cx| first.hypothesis(text, cx));
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                expected
            );
        }
        cx.update(|_, cx| first.finish(cx));
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "前你好后"
        );

        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let edited = sink.borrow().clone().unwrap();
        cx.update(|_, cx| edited.hypothesis(" text", cx));
        cx.simulate_input("!");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.voice.state.read(cx).status()),
            SpeechStatus::Idle,
        );
        let draft = composer.read_with(cx, |composer, cx| composer.draft(cx));
        assert_eq!(draft, "前你好 text!后");
        cx.update(|_, cx| {
            edited.hypothesis("late overwrite", cx);
            edited.finish(cx);
        });
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            draft
        );

        composer.update_in(cx, |composer, window, cx| {
            composer.set_draft("", window, cx);
        });
        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let submitted = sink.borrow().clone().unwrap();
        cx.update(|_, cx| submitted.hypothesis("/model", cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("escape");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.voice.state.read(cx).status()),
            SpeechStatus::Stopping,
        );
        composer.update_in(cx, |composer, window, cx| {
            let input = composer.input.clone();
            composer.submit(&input, false, window, cx);
        });
        cx.update(|_, cx| {
            submitted.hypothesis("late text", cx);
            submitted.finish(cx);
        });
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );
    }
}
