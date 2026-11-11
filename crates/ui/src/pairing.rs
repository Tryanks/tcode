//! Shared pairing form; generation stamps discard superseded pairing results.
use std::net::IpAddr;

use gpui::{App, AppContext as _, Entity, Window};
use tcode_client::pairing::{PairError, PairInvite, PairedHost, parse_pair_url};

use crate::widgets::input::InputState;

/// The form holds the invitation link a machine shows, scanned or pasted.
/// The link is the whole secret, so there is nothing else to type, and a link
/// that parses is submitted the moment it lands: pairing and connecting are
/// one step, not a form and a confirmation. Only when an attempt found no
/// path to the machine does the form ask for one more thing: the machine's
/// IP address, dialed at the port the link names.
pub struct PairForm {
    /// The `tcode://pair?…` link.
    pub invitation: Entity<InputState>,
    /// The machine's IP address, asked for once a link found no path.
    pub address: Entity<InputState>,
    pub busy: bool,
    pub error: Option<String>,
    /// Bumped whenever the form is retargeted; stamps in-flight results.
    pub generation: u64,
    /// The link of the attempt in flight or the one that last failed. A field
    /// that still holds it is not submitted again on every keystroke.
    attempted: Option<String>,
    /// The link whose last attempt opened no path to the machine. Its
    /// secret was never sent, so it may be tried again at a typed address.
    unreachable: Option<String>,
    /// The typed address of the attempt in flight or the last one.
    attempted_address: Option<IpAddr>,
    /// Why the typed address did not reach the machine either. A first
    /// attempt that found no path reports nothing here or in [`Self::error`]:
    /// the address prompt is the whole report, and the transport's cause
    /// would only repeat it.
    address_error: Option<String>,
}

impl PairForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            invitation: cx.new(|cx| InputState::new(window, cx)),
            address: cx.new(|cx| InputState::new(window, cx)),
            busy: false,
            error: None,
            generation: 0,
            attempted: None,
            unreachable: None,
            attempted_address: None,
            address_error: None,
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

    /// Whether to ask for the machine's address: the link in the field is
    /// the one whose last attempt found no path to the machine.
    pub fn needs_address(&self, cx: &App) -> bool {
        self.unreachable
            .as_deref()
            .is_some_and(|link| link == self.invitation.read(cx).value().trim())
    }

    /// Why the typed address did not reach the machine, while the prompt
    /// for it is shown.
    pub fn address_error(&self, cx: &App) -> Option<&str> {
        self.address_error
            .as_deref()
            .filter(|_| self.needs_address(cx))
    }

    /// The address to dial besides what the link names: `Ok(None)` when none
    /// is asked for or the field is empty, `Err` when it is not an IPv4 or
    /// IPv6 address.
    pub fn typed_address(&self, cx: &App) -> Result<Option<IpAddr>, std::net::AddrParseError> {
        if !self.needs_address(cx) {
            return Ok(None);
        }
        let value = self.address.read(cx).value();
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        value.parse().map(Some)
    }

    /// Reset for a fresh attempt and stamp it. Any in-flight pairing result
    /// from the previous generation is discarded when it lands.
    pub fn restart(&mut self) -> u64 {
        self.error = None;
        self.busy = false;
        self.attempted = None;
        self.unreachable = None;
        self.address_error = None;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Mark a submission in flight. Returns the request, the typed address
    /// to dial with it and the attempt's stamp; nothing while the typed
    /// address is not one.
    pub fn begin_pair(&mut self, cx: &App) -> Option<(PairInvite, Option<IpAddr>, u64)> {
        if self.busy {
            return None;
        }
        let value = self.invitation.read(cx).value();
        let request = parse_pair_url(&value)?;
        let address = self.typed_address(cx).ok()?;
        self.attempted = Some(value.trim().to_owned());
        self.attempted_address = address;
        self.busy = true;
        self.error = None;
        self.address_error = None;
        Some((request, address, self.generation))
    }

    /// Apply a pairing result. The paired machine is returned so the caller
    /// connects to it; `None` when the attempt failed (the error is kept for
    /// the page) or belongs to a superseded attempt and was dropped.
    pub fn finish_pair(
        &mut self,
        generation: u64,
        result: Result<PairedHost, PairError>,
    ) -> Option<PairedHost> {
        if generation != self.generation {
            return None;
        }
        self.busy = false;
        match result {
            Ok(host) => {
                self.unreachable = None;
                Some(host)
            }
            Err(PairError::Unreachable(_)) => {
                self.unreachable = self.attempted.clone();
                self.address_error = self.attempted_address.map(|address| {
                    crate::tr!("hosts.pair.address_unreachable", address = address).into_owned()
                });
                None
            }
            Err(error) => {
                self.error = Some(pair_error(&error));
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

    /// Empty the fields for a fresh invitation.
    pub fn clear(&mut self, window: &mut Window, cx: &mut App) {
        self.empty(window, cx);
        self.invitation
            .update(cx, |state, cx| state.focus(window, cx));
    }

    /// The paste page was left. An attempt that has ended leaves nothing
    /// behind, so its failure does not follow the user back to Machines; one
    /// still in flight keeps running and reports there.
    pub fn leave(&mut self, window: &mut Window, cx: &mut App) {
        if !self.busy {
            self.empty(window, cx);
        }
    }

    fn empty(&mut self, window: &mut Window, cx: &mut App) {
        self.error = None;
        self.attempted = None;
        self.unreachable = None;
        self.address_error = None;
        self.address
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.invitation
            .update(cx, |state, cx| state.set_value("", window, cx));
    }
}

pub fn joined_message(host: &PairedHost) -> Option<String> {
    host.space_name
        .as_ref()
        .map(|space| crate::tr!("member.joined", space = space, machine = &host.name).into_owned())
}

/// A pairing failure in the user's language, with what to do about it. A
/// failure that found no path never gets here: the address prompt reports
/// it.
pub fn pair_error(error: &PairError) -> String {
    match error {
        PairError::Invalid => crate::tr!("hosts.pair.rejected").into_owned(),
        PairError::AlreadyMember => crate::tr!("member.already_member").into_owned(),
        PairError::SpaceUnavailable => crate::tr!("member.space_unavailable").into_owned(),
        PairError::Disabled => crate::tr!("hosts.pair.disabled").into_owned(),
        PairError::Protocol(_) => crate::tr!("hosts.pair.unconfirmed").into_owned(),
        PairError::Busy => crate::tr!("hosts.pair.busy").into_owned(),
        PairError::Lost(reason) => crate::tr!("hosts.pair.lost", reason = reason).into_owned(),
        PairError::Unreachable(reason) | PairError::Device(reason) => {
            crate::tr!("hosts.pair.failed", reason = reason).into_owned()
        }
        PairError::Unsupported => {
            crate::tr!("hosts.pair.failed", reason = error.to_string()).into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    fn invite() -> PairInvite {
        PairInvite {
            host_id: "ab".repeat(32),
            secret: "AAECAwQFBgcICQoLDA0ODw".into(),
            traverse: vec!["https://traverse.example/".into()],
            relay: Some("https://relay.example/".into()),
            port: 47420,
            space: None,
            group: None,
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
            let (_, _, generation) = holder.0.begin_pair(cx).expect("a request");
            assert!(!holder.0.should_submit(cx));
            assert!(
                holder
                    .0
                    .finish_pair(generation, Err(PairError::Disabled))
                    .is_none()
            );
            assert!(!holder.0.busy);
            assert!(holder.0.error.is_some());
            assert!(!holder.0.should_submit(cx));
        });
        for rejected in [
            "tcode://pair?v=1&id=abc&code=123456",
            // The previous release's link.
            &format!(
                "tcode://pair?v=2&id={}&secret=AAECAwQFBgcICQoLDA0ODw&name=Studio&addr=10.0.0.4%3A47420",
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
