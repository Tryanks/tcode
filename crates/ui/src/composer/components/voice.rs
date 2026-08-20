//! Live dictation in the composer: the mic toggle button plus the transcript
//! insertion state machine that keeps rewriting the engine's hypothesis in
//! place. macOS only — see `crates/voice` for the engine side.

use super::super::*;

use gpui::SharedString;
use tcode_voice::{DictationEvent, DictationSession};

/// The composer's dictation state.
pub(in super::super) struct Voice {
    /// `tcode_voice::is_supported()`, sampled once per composer: the answer
    /// cannot change while the app runs, and the mic button is absent when it
    /// is false.
    supported: bool,
    session: Option<Dictation>,
}

impl Voice {
    pub(in super::super) fn new() -> Self {
        Self {
            supported: tcode_voice::is_supported(),
            session: None,
        }
    }
}

/// A running session and the byte bookkeeping for its insertion point. The
/// transcript occupies `anchor .. anchor + committed_len + volatile_len`; each
/// `Volatile` rewrites the trailing volatile slice, each `Final` turns it into
/// committed text.
struct Dictation {
    session: DictationSession,
    /// False between `start` and the engine's `Ready` — the first session may
    /// still be downloading speech assets, so the button shows a busy state.
    ready: bool,
    /// Smoothed microphone level (`0.0..=1.0`) shown as a glow behind the mic
    /// icon so the user can see their speech is being picked up.
    level: f32,
    /// Cursor offset (UTF-8 bytes) captured when dictation started.
    anchor: usize,
    committed_len: usize,
    volatile_len: usize,
    /// The input value we last wrote. Any change away from it is a user edit,
    /// which ends the session.
    expected: String,
    /// Pumps engine events (delivered on the engine's own threads) into the UI
    /// thread; dropped with the session.
    _events: Task<()>,
}

/// Where a `len`-byte chunk goes and what the counters become: every chunk
/// replaces the volatile tail, and a final one turns it into committed text.
/// Offsets are UTF-8 bytes, as `TextareaState` selections are.
fn transcript_edit(
    anchor: usize,
    committed_len: usize,
    volatile_len: usize,
    len: usize,
    commit: bool,
) -> (std::ops::Range<usize>, usize, usize) {
    let start = anchor + committed_len;
    let replaced = start..start + volatile_len;
    if commit {
        (replaced, committed_len + len, 0)
    } else {
        (replaced, committed_len, len)
    }
}

impl Composer {
    /// The mic toggle, or `None` when this machine has no dictation engine.
    pub(in super::super) fn render_mic_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.voice.supported {
            return None;
        }
        let dictation = self.voice.session.as_ref();
        let preparing = dictation.is_some_and(|d| !d.ready);
        let tooltip: SharedString = match dictation {
            None => crate::tr!("composer.voice_start"),
            Some(_) if preparing => crate::tr!("composer.voice_preparing"),
            Some(_) => crate::tr!("composer.voice_stop"),
        }
        .into_owned()
        .into();
        // Recording tints the glyph with the danger accent, matching the stop
        // button's "something is live" idiom; preparing stays muted.
        let color = if dictation.is_some() && !preparing {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };

        Some(
            Button::new("voice-mic")
                .ghost()
                .compact()
                .h(px(28.))
                .rounded(crate::material::radius_chip())
                .tooltip(tooltip)
                .child(if preparing {
                    Spinner::new().small().color(color).into_any_element()
                } else {
                    let icon = Icon::empty()
                        .path("icons/mic.svg")
                        .small()
                        .text_color(color);
                    match dictation {
                        // While recording, a glow behind the glyph tracks the
                        // live input level: visible proof the mic hears you.
                        Some(d) => div()
                            .rounded_full()
                            .p(px(2.))
                            .bg(cx.theme().danger.opacity(0.08 + d.level * 0.42))
                            .child(icon)
                            .into_any_element(),
                        None => icon.into_any_element(),
                    }
                })
                .on_click(cx.listener(|this, _, window, cx| this.toggle_dictation(window, cx)))
                .into_any_element(),
        )
    }

    /// Click handler: a second click stops (and finalizes) the running session.
    pub(in super::super) fn toggle_dictation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.stop_dictation(cx) {
            return;
        }
        let locale = rust_i18n::locale();
        let (events_tx, events_rx) = smol::channel::unbounded();
        let session = match tcode_voice::start(
            tcode_voice::preferred_locale(locale.as_ref()),
            Box::new(move |event| {
                let _ = events_tx.try_send(event);
            }),
        ) {
            Ok(session) => session,
            Err(error) => {
                window.push_notification(
                    Notification::error(
                        crate::tr!("composer.voice_error", error = error).into_owned(),
                    ),
                    cx,
                );
                return;
            }
        };

        let (anchor, expected) = self.input.update(cx, |state, cx| {
            // Focus keeps Escape (handled on the card) reachable and leaves the
            // caret where the transcript is about to appear.
            state.focus(window, cx);
            (state.cursor(), state.value().to_string())
        });
        let events = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = events_rx.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        this.on_dictation_event(event, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self.voice.session = Some(Dictation {
            session,
            ready: false,
            level: 0.0,
            anchor,
            committed_len: 0,
            volatile_len: 0,
            expected,
            _events: events,
        });
        cx.notify();
    }

    /// Stop the running session, keeping every character inserted so far.
    /// Returns whether there was one (Escape uses this to claim the keystroke).
    pub(in super::super) fn stop_dictation(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(dictation) = self.voice.session.take() else {
            return false;
        };
        dictation.session.stop();
        cx.notify();
        true
    }

    /// Typing, pasting or undoing while dictating ends the session in place:
    /// the anchor no longer describes the text, and merging a live hypothesis
    /// with concurrent edits is not worth the ambiguity.
    pub(in super::super) fn stop_dictation_on_user_edit(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value();
        if self
            .voice
            .session
            .as_ref()
            .is_some_and(|dictation| dictation.expected != value.as_ref())
        {
            self.stop_dictation(cx);
        }
    }

    fn on_dictation_event(
        &mut self,
        event: DictationEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DictationEvent::Ready => {
                if let Some(dictation) = self.voice.session.as_mut() {
                    dictation.ready = true;
                    cx.notify();
                }
            }
            DictationEvent::Level(level) => {
                if let Some(dictation) = self.voice.session.as_mut() {
                    // Fast attack, slow decay: peaks register instantly and
                    // fade out instead of flickering at the callback rate.
                    dictation.level = level.max(dictation.level * 0.8);
                    cx.notify();
                }
            }
            DictationEvent::Volatile(text) => self.insert_transcript(text, false, window, cx),
            DictationEvent::Final(text) => self.insert_transcript(text, true, window, cx),
            DictationEvent::Error(message) => {
                self.stop_dictation(cx);
                window.push_notification(
                    Notification::error(
                        crate::tr!("composer.voice_error", error = message).into_owned(),
                    ),
                    cx,
                );
            }
            DictationEvent::Ended => {
                self.voice.session = None;
                cx.notify();
            }
        }
    }

    /// Replace the current volatile range with `text`, committing it when the
    /// engine says it is final.
    fn insert_transcript(
        &mut self,
        text: String,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.input.clone();
        let Some(dictation) = self.voice.session.as_mut() else {
            return;
        };
        let (range, committed_len, volatile_len) = transcript_edit(
            dictation.anchor,
            dictation.committed_len,
            dictation.volatile_len,
            text.len(),
            commit,
        );
        input.update(cx, |state, cx| {
            state.set_selected_range(range, cx);
            state.replace(text, window, cx);
        });
        dictation.committed_len = committed_len;
        dictation.volatile_len = volatile_len;
        // The replacement emits `Change` like any edit; record the result so
        // the edit guard can tell our own writes from the user's.
        dictation.expected = input.read(cx).value().to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::transcript_edit;

    /// A hypothesis is rewritten in place until it is finalized, after which
    /// the next one starts where the committed text ends.
    #[test]
    fn each_chunk_replaces_only_the_volatile_tail() {
        let (anchor, mut committed, mut volatile) = (5, 0, 0);

        let (range, c, v) = transcript_edit(anchor, committed, volatile, 2, false);
        assert_eq!(range, 5..5);
        (committed, volatile) = (c, v);

        // A longer hypothesis replaces the shorter one it grew from.
        let (range, c, v) = transcript_edit(anchor, committed, volatile, 6, false);
        assert_eq!(range, 5..7);
        (committed, volatile) = (c, v);

        let (range, c, v) = transcript_edit(anchor, committed, volatile, 6, true);
        assert_eq!(range, 5..11);
        assert_eq!((c, v), (6, 0));
        (committed, volatile) = (c, v);

        // The next hypothesis is inserted after the committed text.
        let (range, ..) = transcript_edit(anchor, committed, volatile, 3, false);
        assert_eq!(range, 11..11);
    }
}
