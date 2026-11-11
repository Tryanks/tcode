//! System dictation, written into the draft as the recognizer revises it.

use super::super::*;
use client::{DictationEvent as SpeechEvent, SpeechState, SpeechStatus};
use gpui::SharedString;
use gpui_base::input::InputEditsEvent;
use tcode_client::speech::Revision;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

mod client;

pub(in super::super) struct Voice {
    state: Entity<SpeechState>,
    locale: &'static str,
    insertion: Option<Insertion>,
    /// A send that arrived mid-dictation, with its steer flag. Platforms
    /// recognize behind the audio, so sending at once would cut off speech
    /// that has been spoken but not yet transcribed.
    pending_submit: Option<bool>,
}

/// Where a dictation writes. Its settled text stays as written; each
/// revision replaces only the hypothesis after it.
struct Insertion {
    /// Where the hypothesis starts, after the settled text.
    start: usize,
    /// The hypothesis as written.
    hypothesis: String,
    /// The replaced range and new end of each write the draft has not
    /// reported yet. A reported edit that is not one of them is the user's,
    /// and ends dictation.
    writes: std::collections::VecDeque<(std::ops::Range<usize>, usize)>,
    _edits: Subscription,
}

fn speech_locale() -> &'static str {
    let locale = rust_i18n::locale();
    if locale.starts_with("zh") {
        "zh-CN"
    } else if locale.starts_with("ja") {
        "ja-JP"
    } else {
        "en-US"
    }
}

fn speech_state(cx: &mut Context<SpeechState>) -> SpeechState {
    client::configure(SpeechState::new(cx), speech_locale(), cx)
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
        // A permission granted or revoked in System Settings shows once the
        // user comes back to Tcode.
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.voice
                    .state
                    .update(cx, |state, cx| state.refresh_permission(cx));
            }
        }));
        subscriptions.push(cx.on_release(|this, cx| {
            this.voice.insertion = None;
            this.voice.state.update(cx, |state, cx| state.cancel(cx));
        }));
        Self {
            state,
            locale: speech_locale(),
            insertion: None,
            pending_submit: None,
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
                *state = client::configure(previous, locale, cx);
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
        let recording = status == SpeechStatus::Recording;
        let level = state.level();
        // The glyph turns red and gains a halo while it records, the halo
        // following the voice's level smoothly rather than each raw reading;
        // a spinner cross-fades in while it connects or stops.
        let mark = crate::motion::Sampled::new(move |window, cx| {
            let theme = cx.theme().clone();
            let color = gpui_base::motion::transition(
                "voice-mic-color",
                if recording {
                    theme.danger
                } else {
                    theme.muted_foreground
                },
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            let halo = gpui_base::motion::spring(
                "voice-mic-halo",
                if recording { 0.08 + level * 0.42 } else { 0. },
                gpui_base::motion::Spring::new(GLIDE).with_epsilon(0.25),
                window,
                cx,
            );
            crate::motion::cross_fade_part(
                "voice-mic-mark",
                preparing,
                CHANGE,
                window,
                cx,
                move |preparing, _, cx| {
                    if *preparing {
                        return Spinner::new().color(color).into_any_element();
                    }
                    div()
                        .relative()
                        .child(
                            div()
                                .absolute()
                                .inset(-cx.theme().tokens.spacing.xxs)
                                .rounded_full()
                                .bg(cx.theme().danger.opacity(halo.max(0.))),
                        )
                        .child(Icon::empty().path("icons/mic.svg").text_color(color))
                        .into_any_element()
                },
            )
        });
        Some(
            Button::new("voice-mic")
                .ghost()
                .chip()
                .disabled(!available)
                .tooltip(tooltip)
                .child(mark)
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

    /// Stops an active dictation and defers the send until its final
    /// transcript is in the draft.
    pub(in super::super) fn submit_after_dictation(
        &mut self,
        steer: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.voice.state.read(cx).status().is_active() {
            return false;
        }
        self.voice.pending_submit = Some(steer);
        self.voice.state.update(cx, |state, cx| state.stop(cx));
        true
    }

    pub(in super::super) fn abort_dictation(&mut self, cx: &mut Context<Self>) {
        self.voice.insertion = None;
        self.voice.pending_submit = None;
        self.voice.state.update(cx, |state, cx| state.cancel(cx));
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
                    self.focus(window, cx);
                    let edits =
                        cx.subscribe(&self.input, |this, _, event: &InputEditsEvent, cx| {
                            this.on_draft_edited(event, cx);
                        });
                    self.voice.insertion = Some(Insertion {
                        start: self.input.read(cx).cursor(),
                        hypothesis: String::new(),
                        writes: Default::default(),
                        _edits: edits,
                    });
                }
            }
            SpeechEvent::Revised(revision) => self.write_revision(revision, window, cx),
            SpeechEvent::Final => {
                self.voice.insertion = None;
                if let Some(steer) = self.voice.pending_submit.take() {
                    let input = self.input.clone();
                    self.submit(&input, steer, window, cx);
                }
            }
            SpeechEvent::Error(error) => {
                self.voice.insertion = None;
                self.voice.pending_submit = None;
                let notification = Notification::error(
                    crate::tr!("composer.voice_error", error = error).into_owned(),
                );
                #[cfg(target_os = "macos")]
                let notification = match error {
                    tcode_client::speech::SpeechError::Recognizer(error) if error.ends_with("(kLSRErrorDomain 201)") =>
                    {
                        Notification::error(crate::tr!("composer.voice_enable_dictation"))
                            .id1::<Voice>("dictation-disabled")
                            .autohide(false)
                            .action(|_, _, _| {
                                Button::new("open-dictation-settings")
                                    .outline()
                                    .label(crate::tr!("composer.voice_open_dictation_settings"))
                                    .on_click(|_, _, cx| {
                                        cx.open_url(
                                            "x-apple.systempreferences:com.apple.preference.keyboard?Dictation",
                                        );
                                    })
                            })
                    }
                    _ => notification,
                };
                window.push_notification(notification, cx);
            }
            SpeechEvent::Cancelled => {
                self.voice.insertion = None;
                self.voice.pending_submit = None;
            }
        }
        cx.notify();
    }

    /// Settled text is written once and each revision replaces only the
    /// hypothesis after it, so a write, and the undo step it leaves, is the
    /// size of the revision rather than of the session.
    fn write_revision(&mut self, revision: &Revision, window: &mut Window, cx: &mut Context<Self>) {
        let Some(insertion) = self.voice.insertion.as_mut() else {
            return;
        };
        if revision.settled.is_empty() && revision.hypothesis == insertion.hypothesis {
            return;
        }
        let range = insertion.start..insertion.start + insertion.hypothesis.len();
        let text = format!("{}{}", revision.settled, revision.hypothesis);
        insertion
            .writes
            .push_back((range.clone(), range.start + text.len()));
        insertion.start += revision.settled.len();
        insertion.hypothesis.clone_from(&revision.hypothesis);
        self.input.update(cx, |input, cx| {
            input.set_selected_range(range, cx);
            input.replace(text, window, cx);
        });
    }

    fn on_draft_edited(&mut self, event: &InputEditsEvent, cx: &mut Context<Self>) {
        let Some(insertion) = self.voice.insertion.as_mut() else {
            return;
        };
        let written = match (event.edits(), insertion.writes.front()) {
            ([(edit, _)], Some((range, end))) => {
                edit.start_byte == range.start
                    && edit.old_end_byte == range.end
                    && edit.new_end_byte == *end
            }
            _ => false,
        };
        if written {
            insertion.writes.pop_front();
        } else {
            self.abort_dictation(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};
    use std::cell::RefCell;
    use tcode_client::speech::{SpeechEvent as PlatformEvent, SpeechRecognizer, SpeechSession};

    struct Recognizer(Rc<RefCell<Option<async_channel::Sender<PlatformEvent>>>>);
    struct Recognition;

    impl SpeechRecognizer for Recognizer {
        fn is_available(&self, _: &str) -> bool {
            true
        }
        fn start(
            &self,
            _: &str,
            events: async_channel::Sender<PlatformEvent>,
        ) -> Result<Box<dyn SpeechSession>, tcode_client::speech::SpeechError> {
            events.try_send(PlatformEvent::Ready).unwrap();
            *self.0.borrow_mut() = Some(events);
            Ok(Box::new(Recognition))
        }
    }

    impl SpeechSession for Recognition {
        fn finish(&mut self) {}
    }

    struct Fixture {
        host: tcode_runtime::pipe::SpawnedHost,
        driver: crate::store::tests::LocalHostDriver,
        composer: Entity<Composer>,
        sink: Rc<RefCell<Option<async_channel::Sender<PlatformEvent>>>>,
    }

    fn dictating_composer<'a>(
        cx: &'a mut TestAppContext,
        name: &str,
    ) -> (Fixture, &'a mut gpui::VisualTestContext) {
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(crate::store::tests::scratch_root(name))
                .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let session_id = smol::block_on(host.update_state_for_test(|state, cx| {
            state.start_draft("dictation".into(), std::env::temp_dir(), cx)
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, "selected conversation", |cx| {
            store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.chat_loading_for(store.sole_thread().as_deref(), cx)
            })
        });
        let sink = Rc::new(RefCell::new(None::<async_channel::Sender<PlatformEvent>>));
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let composer = Composer::new(store.clone(), Some(session_id), window, cx);
            composer.voice.state.update(cx, |state, cx| {
                *state = SpeechState::new(cx).recognizer(Rc::new(Recognizer(sink.clone())));
            });
            composer
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (
            Fixture {
                host,
                driver,
                composer,
                sink,
            },
            cx,
        )
    }

    #[gpui::test]
    fn dictation_revises_the_draft_and_a_send_waits_for_its_final_transcript(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (
            Fixture {
                host,
                mut driver,
                composer,
                sink,
            },
            cx,
        ) = dictating_composer(cx, "dictation-test");
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
            first
                .try_send(PlatformEvent::Hypothesis(text.into()))
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                expected
            );
        }
        first.try_send(PlatformEvent::Finished).unwrap();
        cx.run_until_parked();
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
        edited
            .try_send(PlatformEvent::Hypothesis(" text".into()))
            .unwrap();
        cx.run_until_parked();
        cx.simulate_input("!");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.voice.state.read(cx).status()),
            SpeechStatus::Idle,
        );
        let draft = composer.read_with(cx, |composer, cx| composer.draft(cx));
        assert_eq!(draft, "前你好 text!后");
        _ = edited.try_send(PlatformEvent::Hypothesis("late overwrite".into()));
        _ = edited.try_send(PlatformEvent::Finished);
        cx.run_until_parked();
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
        submitted
            .try_send(PlatformEvent::Hypothesis("/mod".into()))
            .unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("escape");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.voice.state.read(cx).status()),
            SpeechStatus::Stopping,
        );
        let picker = composer.read_with(cx, |composer, _| composer.model_picker_token);
        composer.update_in(cx, |composer, window, cx| {
            let input = composer.input.clone();
            composer.submit(&input, false, window, cx);
        });
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "/mod"
        );
        submitted
            .try_send(PlatformEvent::Phrase("/model".into()))
            .unwrap();
        submitted.try_send(PlatformEvent::Finished).unwrap();
        cx.run_until_parked();
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );
        assert_ne!(
            composer.read_with(cx, |composer, _| composer.model_picker_token),
            picker,
            "the send must carry the final transcript"
        );
        _ = submitted.try_send(PlatformEvent::Hypothesis("late text".into()));
        cx.run_until_parked();
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );

        cx.update(|window, cx| {
            crate::window_seam::force_mobile_layout(cx);
            window.blur(cx);
        });
        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let touch = sink.borrow().clone().unwrap();
        touch
            .try_send(PlatformEvent::Hypothesis("no keyboard".into()))
            .unwrap();
        cx.run_until_parked();
        composer.update_in(cx, |composer, window, cx| {
            assert!(
                !composer.input.read(cx).focus_handle(cx).is_focused(window),
                "dictation on a touch layout must not raise the software keyboard"
            );
            assert_eq!(composer.draft(cx), "no keyboard");
        });
        driver.shutdown(cx, &host);
    }

    /// A long session, Latin and CJK, as the recognizer reports it: each
    /// result revises only the hypothesis after the settled text, so what a
    /// result asks of the draft does not grow with the session, and what
    /// queued while speech stopped lands as one edit.
    #[gpui::test]
    fn a_long_dictation_writes_each_result_at_the_cost_of_the_result(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (
            Fixture {
                host,
                mut driver,
                composer,
                sink,
            },
            cx,
        ) = dictating_composer(cx, "dictation-long");
        composer.update_in(cx, |composer, window, cx| {
            composer.set_draft("前后", window, cx);
            composer.input.update(cx, |input, cx| {
                input.set_selected_range(3..3, cx);
            });
        });
        // Bytes each edit replaced and inserted, as the draft reports them.
        let edits = Rc::new(RefCell::new(Vec::<usize>::new()));
        let _edits = cx.update(|_, cx| {
            let edits = edits.clone();
            let input = composer.read(cx).input.clone();
            cx.subscribe(&input, move |_, event: &InputEditsEvent, _| {
                for (edit, _) in event.edits() {
                    edits.borrow_mut().push(
                        edit.old_end_byte - edit.start_byte + edit.new_end_byte - edit.start_byte,
                    );
                }
            })
        });
        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let speech = sink.borrow().clone().unwrap();

        let latin = [
            " dictation",
            " keeps",
            " the",
            " composer",
            " responsive",
            " while",
            " speaking",
        ];
        let cjk = [
            "我们今天",
            "讨论一下",
            "这个问题",
            "的解决方案",
            "并且确认",
            "细节",
        ];
        let mut settled = String::new();
        let mut results = Vec::new();
        for phrase in 0..14 {
            let words: &[&str] = if phrase % 2 == 0 { &latin } else { &cjk };
            let mut text = String::new();
            for word in 0..24 {
                text.push_str(words[(phrase + word) % words.len()]);
                if word % 2 == 1 && word < 23 {
                    results.push(PlatformEvent::Hypothesis(text.clone()));
                }
            }
            settled.push_str(&text);
            results.push(PlatformEvent::Phrase(text));
        }
        let backlog = results.split_off(150);

        let mut costs = Vec::new();
        for result in results {
            let before = edits.borrow().len();
            speech.try_send(result).unwrap();
            cx.run_until_parked();
            costs.push(edits.borrow()[before..].iter().sum::<usize>());
        }
        let (first, last) = (
            costs[..50].iter().sum::<usize>(),
            costs[costs.len() - 50..].iter().sum::<usize>(),
        );
        assert!(
            last <= first * 2,
            "the last 50 results asked the draft for {last} bytes, the first 50 for {first}"
        );

        composer.update_in(cx, |composer, _, cx| {
            assert!(composer.stop_dictation(cx));
        });
        let before = edits.borrow().len();
        for result in backlog {
            speech.try_send(result).unwrap();
        }
        speech.try_send(PlatformEvent::Finished).unwrap();
        cx.run_until_parked();
        assert_eq!(edits.borrow().len() - before, 1, "the backlog is one edit");
        composer.read_with(cx, |composer, cx| {
            assert!(settled.chars().count() > 1_500);
            assert_eq!(composer.draft(cx), format!("前{settled}后"));
            assert_eq!(composer.input.read(cx).cursor(), "前".len() + settled.len());
            assert_eq!(composer.voice.state.read(cx).status(), SpeechStatus::Idle);
        });
        driver.shutdown(cx, &host);
    }
}
