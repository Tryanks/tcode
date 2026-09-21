//! Shared pairing form; generation stamps discard superseded pairing results.
use gpui::{App, AppContext as _, Entity, Window};
use tcode_client::pairing::{PairInvite, PairedHost, parse_pair_url};

use crate::widgets::input::InputState;

/// The form holds one thing: the invitation link a machine shows, scanned or
/// pasted. The link is the whole secret, so there is nothing else to type.
pub struct PairForm {
    /// Paired but waiting for the user to connect.
    pub paired: Option<PairedHost>,
    /// The `tcode://pair?…` link.
    pub invitation: Entity<InputState>,
    pub busy: bool,
    pub error: Option<String>,
    /// Bumped whenever the form is retargeted; stamps in-flight results.
    pub generation: u64,
    /// Whether the shell has already installed its paste listeners.
    pub listening: bool,
}

impl PairForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            paired: None,
            invitation: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::tr!("hosts.pair.invitation_placeholder").into_owned())
            }),
            busy: false,
            error: None,
            generation: 0,
            listening: false,
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

    /// Reset for a fresh attempt and stamp it. Any in-flight pairing result
    /// from the previous generation is discarded when it lands.
    pub fn restart(&mut self) -> u64 {
        self.paired = None;
        self.error = None;
        self.busy = false;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Mark a submission in flight. Returns the request and its stamp.
    pub fn begin_pair(&mut self, cx: &App) -> Option<(PairInvite, u64)> {
        if self.busy {
            return None;
        }
        let request = self.request(cx)?;
        self.busy = true;
        self.error = None;
        Some((request, self.generation))
    }

    /// Apply a pairing result. Returns `false` when it belongs to a superseded
    /// attempt and was dropped.
    pub fn finish_pair(
        &mut self,
        generation: u64,
        result: Result<PairedHost, String>,
        address: &str,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.busy = false;
        match result {
            Ok(host) => {
                self.paired = Some(host);
            }
            Err(error) => self.error = Some(pair_error(&error, address)),
        }
        true
    }

    /// Take the paired host so the shell can connect to it.
    pub fn take_paired(&mut self) -> Option<PairedHost> {
        self.paired.take()
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
        self.invitation.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
    }
}

/// Interpret known transport reasons here, where the recovery advice can be
/// localized. A lost reply cannot establish whether a single-use code was used.
pub fn pair_error(error: &str, address: &str) -> String {
    let lower = error.to_ascii_lowercase();
    match lower.trim() {
        "could not authenticate the machine at any invited address"
        | "pairing identity changed" => crate::tr!("hosts.pair.identity_error").into_owned(),
        "invalid pairing invitation"
        | "missing pairing identity"
        | "invalid pairing identity"
        | "invalid insecure pairing alternative"
        | "invalid pairing request"
        | "malformed pairing request" => crate::tr!("hosts.pair.bad_invite").into_owned(),
        "incomplete pairing response"
        | "pairing response timed out"
        | "invalid pairing response"
        | "incomplete http response"
        | "invalid http response" => crate::tr!("hosts.pair.unconfirmed").into_owned(),
        _ if lower.starts_with("invalid pairing response") => {
            crate::tr!("hosts.pair.unconfirmed").into_owned()
        }
        "pairing_disabled" | "pairing disabled" => crate::tr!("hosts.pair.disabled").into_owned(),
        "pairing rejected" => crate::tr!("hosts.pair.bad_code").into_owned(),
        _ if lower.contains("403")
            || lower.contains("401")
            || lower.contains("expired")
            || lower.contains("invalid code") =>
        {
            // Older HTTP clients retain only the status, so 403 cannot tell
            // an expired code from a host that disabled new pairings.
            crate::tr!("hosts.pair.bad_code").into_owned()
        }
        _ if ["timeout", "timed out", "refused", "connect", "dns"]
            .iter()
            .any(|needle| lower.contains(needle)) =>
        {
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
        for (errors, key) in [
            (
                &[
                    "could not authenticate the machine at any invited address",
                    "pairing identity changed",
                ][..],
                "hosts.pair.identity_error",
            ),
            (
                &[
                    "invalid pairing invitation",
                    "missing pairing identity",
                    "invalid pairing identity",
                    "invalid insecure pairing alternative",
                    "malformed pairing request",
                ][..],
                "hosts.pair.bad_invite",
            ),
            (
                &[
                    "incomplete pairing response",
                    "pairing response timed out",
                    "invalid pairing response",
                    "incomplete HTTP response",
                ][..],
                "hosts.pair.unconfirmed",
            ),
            (&["pairing_disabled"][..], "hosts.pair.disabled"),
            (
                &[
                    "HTTP/1.1 403 Forbidden",
                    "HTTP 401",
                    "invalid or expired pairing code",
                    "pairing rejected",
                ][..],
                "hosts.pair.bad_code",
            ),
        ] {
            for error in errors {
                assert_eq!(
                    pair_error(error, "192.168.1.161:47420"),
                    crate::tr!(key).into_owned(),
                    "wrong recovery advice for {error}",
                );
            }
        }
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
            assert!(
                !holder.0.finish_pair(stale, Err("HTTP 403".into()), "a:1"),
                "a pairing answer from the previous attempt must be dropped"
            );
            assert_eq!(holder.0.error, None);

            assert!(holder.0.finish_pair(current, Err("HTTP 403".into()), "a:1"));
            assert_eq!(
                holder.0.error.as_deref(),
                Some(crate::tr!("hosts.pair.bad_code").into_owned().as_str()),
                "a status-only rejection cannot diagnose an expired or malformed code"
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
