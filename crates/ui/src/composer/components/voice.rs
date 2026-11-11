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
        } else if status == SpeechStatus::Stopping {
            crate::tr!("composer.voice_stop_waiting")
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
