//! Shared pairing form; generation stamps discard superseded pairing results.
use gpui::{App, AppContext as _, Entity, Window};
use tcode_client::pairing::{PairInvite, PairedHost, parse_pair_url};

use crate::widgets::input::InputState;

/// The form holds one thing: the invitation link a machine shows, scanned or
/// pasted. The link is the whole secret, so there is nothing else to type, and
/// a link that parses is submitted the moment it lands: pairing and
/// connecting are one step, not a form and a confirmation.
pub struct PairForm {
    /// The `tcode://pair?…` link.
    pub invitation: Entity<InputState>,
    pub busy: bool,
    pub error: Option<String>,
    /// Bumped whenever the form is retargeted; stamps in-flight results.
    pub generation: u64,
    /// The link of the attempt in flight or the one that last failed. A field
    /// that still holds it is not submitted again on every keystroke.
    attempted: Option<String>,
}

impl PairForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            invitation: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::tr!("hosts.pair.invitation_placeholder").into_owned())
            }),
            busy: false,
            error: None,
            generation: 0,
            attempted: None,
        }
    }

    /// The invite in the field, once it is one.
    pub fn request(&self, cx: &App) -> Option<PairInvite> {
        parse_pair_url(&self.invitation.read(cx).value())
    }

    /// Whether the field holds something that is not an invitation, so the
    /// page can say so before the user looks for a disabled button.
    pub fn invalid(&self, cx: &App) -> bool {
        let value = self.invitation.read(cx).value();
        !value.trim().is_empty() && parse_pair_url(&value).is_none()
    }

    /// Whether the field holds an invitation that has not been tried yet: the
    /// signal to submit without waiting for a button.
    pub fn should_submit(&self, cx: &App) -> bool {
        if self.busy {
            return false;
        }
        let value = self.invitation.read(cx).value();
        parse_pair_url(&value).is_some() && self.attempted.as_deref() != Some(value.trim())
    }

    /// Reset for a fresh attempt and stamp it. Any in-flight pairing result
    /// from the previous generation is discarded when it lands.
    pub fn restart(&mut self) -> u64 {
        self.error = None;
        self.busy = false;
        self.attempted = None;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Mark a submission in flight. Returns the request and its stamp.
    pub fn begin_pair(&mut self, cx: &App) -> Option<(PairInvite, u64)> {
        if self.busy {
            return None;
        }
        let value = self.invitation.read(cx).value();
        let request = parse_pair_url(&value)?;
        self.attempted = Some(value.trim().to_owned());
        self.busy = true;
        self.error = None;
        Some((request, self.generation))
    }

    /// Apply a pairing result. The paired machine is returned so the caller
    /// connects to it; `None` when the attempt failed (the error is kept for
    /// the page) or belongs to a superseded attempt and was dropped.
    pub fn finish_pair(
        &mut self,
        generation: u64,
        result: Result<PairedHost, String>,
        address: &str,
    ) -> Option<PairedHost> {
        if generation != self.generation {
            return None;
        }
        self.busy = false;
        match result {
            Ok(host) => Some(host),
            Err(error) => {
                self.error = Some(pair_error(&error, address));
                None
            }
        }
    }

    /// Adopt a scanned or pasted `tcode://pair?…` link. Returns `false`, and
    /// leaves the field alone, when it is not one.
    pub fn fill_invite(&mut self, value: &str, window: &mut Window, cx: &mut App) -> bool {
        if parse_pair_url(value).is_none() {
            return false;
        }
        self.invitation
            .update(cx, |state, cx| state.set_value(value.trim(), window, cx));
        self.error = None;
        true
    }

    /// Empty the field for a fresh invitation.
    pub fn clear(&mut self, window: &mut Window, cx: &mut App) {
        self.error = None;
        self.attempted = None;
        self.invitation.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
    }
}

/// Interpret the transport's pairing failures here, where the recovery
/// advice can be localized. The wording is `PairError`'s `Display` in
/// `crates/traverse/src/client.rs`; anything else is shown as it is.
pub fn pair_error(error: &str, address: &str) -> String {
    let lower = error.to_ascii_lowercase();
    match lower.trim() {
        "invalid or expired invitation" => crate::tr!("hosts.pair.rejected").into_owned(),
        "pairing_disabled" => crate::tr!("hosts.pair.disabled").into_owned(),
        _ if lower.starts_with("invalid pairing response") => {
            crate::tr!("hosts.pair.unconfirmed").into_owned()
        }
        _ if lower.starts_with("could not connect to the machine") => {
            crate::tr!("hosts.pair.network_error", address = address).into_owned()
        }
        _ => crate::tr!("hosts.pair.failed", reason = error).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[test]
    fn pairing_failures_preserve_the_recovery_action_instead_of_exposing_transport_wording() {
        for (error, key) in [
            ("invalid or expired invitation", "hosts.pair.rejected"),
            ("pairing_disabled", "hosts.pair.disabled"),
            (
                "invalid pairing response: unexpected reply Refused",
                "hosts.pair.unconfirmed",
            ),
        ] {
            assert_eq!(
                pair_error(error, "ab12cd34"),
                crate::tr!(key).into_owned(),
                "wrong recovery advice for {error}",
            );
        }
        assert_eq!(
            pair_error(
                "could not connect to the machine: connection timed out",
                "ab12cd34"
            ),
            crate::tr!("hosts.pair.network_error", address = "ab12cd34").into_owned()
        );
        assert_eq!(
            pair_error(
                "the machine could not record the pairing; try again",
                "ab12cd34"
            ),
            crate::tr!(
                "hosts.pair.failed",
                reason = "the machine could not record the pairing; try again"
            )
            .into_owned(),
            "an unfamiliar reason is shown as it is"
        );
    }

    fn invite() -> PairInvite {
        PairInvite {
            host_id: "ab".repeat(32),
            name: "Studio".into(),
            secret: "AAECAwQFBgcICQoLDA0ODw".into(),
            traverse: Some("https://traverse.example/".into()),
            relay: Some("https://relay.example/".into()),
            addrs: vec!["10.0.0.4:47420".into()],
        }
    }

    /// The link a machine shows is the whole request, however it arrived;
    /// anything else in the field is flagged and never sent.
    #[gpui::test]
    fn a_pasted_link_is_the_whole_request_and_anything_else_is_flagged(cx: &mut TestAppContext) {
        let (form, cx) = cx.add_window_view(|window, cx| Holder(PairForm::new(window, cx)));
        let url = tcode_client::pairing::pair_url(&invite());
        cx.update(|window, cx| {
            form.update(cx, |holder, cx| {
                holder.0.invitation.update(cx, |state, cx| {
                    state.set_value(format!("  {url}\n"), window, cx);
                });
            });
        });
        form.read_with(cx, |holder, cx| {
            assert_eq!(holder.0.request(cx), Some(invite()));
            assert!(!holder.0.invalid(cx));
            assert!(
                holder.0.should_submit(cx),
                "a link that parses is submitted without a button"
            );
        });
        // Once tried, the same link is not sent again while it sits in the
        // field: not while the attempt is in flight, and not after it failed.
        form.update(cx, |holder, cx| {
            let (_, generation) = holder.0.begin_pair(cx).expect("a request");
            assert!(!holder.0.should_submit(cx));
            assert!(
                holder
                    .0
                    .finish_pair(generation, Err("pairing_disabled".into()), "Studio")
                    .is_none()
            );
            assert!(!holder.0.busy);
            assert!(holder.0.error.is_some());
            assert!(!holder.0.should_submit(cx));
        });
        for rejected in [
            "tcode://pair?v=1&id=abc&code=123456",
            &format!(
                "tcode://pair?v=2&id={}&code=123456&name=Studio",
                "ab".repeat(32)
            ),
            &"ab".repeat(32),
            "https://example.com/pair",
        ] {
            cx.update(|window, cx| {
                form.update(cx, |holder, cx| {
                    assert!(!holder.0.fill_invite(rejected, window, cx), "{rejected:?}");
                    holder.0.invitation.update(cx, |state, cx| {
                        state.set_value(rejected, window, cx);
                    });
                });
            });
            form.read_with(cx, |holder, cx| {
                assert_eq!(holder.0.request(cx), None, "{rejected:?}");
                assert!(holder.0.invalid(cx), "{rejected:?}");
            });
        }
        cx.update(|window, cx| {
            form.update(cx, |holder, cx| holder.0.clear(window, cx));
        });
        form.read_with(cx, |holder, cx| {
            assert!(!holder.0.invalid(cx), "an empty field is not an error");
            assert_eq!(holder.0.request(cx), None);
            assert!(!holder.0.should_submit(cx));
        });
    }

    /// Pairing outlives the attempt that started it. A reply from a
    /// superseded attempt is dropped, so reopening the form cannot be
    /// retargeted by an answer the user has already moved on from.
    #[gpui::test]
    fn results_from_a_superseded_attempt_are_dropped(cx: &mut TestAppContext) {
        let (form, cx) = cx.add_window_view(|window, cx| Holder(PairForm::new(window, cx)));

        let stale = form.update(cx, |holder, _| holder.0.restart());
        let current = form.update(cx, |holder, _| holder.0.restart());
        assert_ne!(stale, current);

        form.update(cx, |holder, _| {
            let rejected = || Err("invalid or expired invitation".into());
            assert!(
                holder.0.finish_pair(stale, rejected(), "a:1").is_none(),
                "a pairing answer from the previous attempt must be dropped"
            );
            assert_eq!(holder.0.error, None);

            assert!(holder.0.finish_pair(current, rejected(), "a:1").is_none());
            assert_eq!(
                holder.0.error.as_deref(),
                Some(crate::tr!("hosts.pair.rejected").into_owned().as_str())
            );
        });
    }

    /// A view is only needed because the inputs live in a window.
    struct Holder(PairForm);

    impl gpui::Render for Holder {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
}
