//! Shared pairing form; generation stamps discard superseded pairing results.
//!
//! TODO(traverse): the address field now holds the machine id; the Machines
//! page needs a redesign around scanning, nearby machines and fingerprints.
use gpui::{App, AppContext as _, Entity, Window};
use tcode_client::host::DiscoveredHost;
use tcode_client::pairing::{
    PairInvite, PairedHost, is_pairing_code, parse_pair_url, valid_host_id,
};

use crate::widgets::input::InputState;

/// Default UDP port a hosting desktop binds its Traverse endpoint to.
pub const DEFAULT_REMOTE_PORT: u16 = 47_420;

pub struct PairForm {
    fixed_endpoint: Option<String>,
    invite: Option<PairInvite>,
    pub discovered: Vec<DiscoveredHost>,
    pub browsing: bool,
    /// Paired but waiting for the user to connect.
    pub paired: Option<PairedHost>,
    pub address: Entity<InputState>,
    pub code: Entity<InputState>,
    pub busy: bool,
    pub error: Option<String>,
    /// An invite filled the fields in, so the shell can say so.
    pub filled: bool,
    /// Bumped whenever the form is retargeted; stamps in-flight results.
    pub generation: u64,
    /// Whether the shell has already installed its paste listeners.
    pub listening: bool,
}

impl PairForm {
    /// `fixed` is [`tcode_client::host::ClientHost::fixed_pairing_endpoint`]:
    /// `Some` fixes the origin and hides the address field.
    pub fn new(fixed: Option<String>, window: &mut Window, cx: &mut App) -> Self {
        Self {
            discovered: Vec::new(),
            browsing: false,
            paired: None,
            address: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::tr!("hosts.pair.address_placeholder").into_owned())
                    .default_value(fixed.as_ref().cloned().unwrap_or_default())
            }),
            code: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::tr!("hosts.pair.code_placeholder").into_owned())
            }),
            busy: false,
            error: None,
            filled: false,
            generation: 0,
            fixed_endpoint: fixed,
            invite: None,
            listening: false,
        }
    }

    /// Whether this client can only pair with one endpoint (a browser).
    pub fn has_fixed_endpoint(&self) -> bool {
        self.fixed_endpoint.is_some()
    }

    /// Whether the "nearby hosts" list is worth showing at all.
    pub fn show_discovery(&self) -> bool {
        !self.has_fixed_endpoint() && (self.browsing || !self.discovered.is_empty())
    }

    /// Validate the machine id and six-digit code before submission. The
    /// code only ever goes to the id in the field; a scanned invite or a
    /// discovered machine with that same id contributes its addresses.
    pub fn request(&self, cx: &App) -> Option<PairInvite> {
        let code = self.code.read(cx).value().to_string();
        if !is_pairing_code(&code) {
            return None;
        }
        if self.fixed_endpoint.is_some() {
            return Some(PairInvite {
                host_id: String::new(),
                name: String::new(),
                code,
                traverse: None,
                relay: None,
                addrs: Vec::new(),
            });
        }
        let host_id = self.address.read(cx).value().trim().to_ascii_lowercase();
        if !valid_host_id(&host_id) {
            return None;
        }
        if let Some(invite) = self
            .invite
            .as_ref()
            .filter(|invite| invite.host_id == host_id)
        {
            return Some(PairInvite {
                code,
                ..invite.clone()
            });
        }
        let nearby = self.discovered.iter().find(|host| host.host_id == host_id);
        Some(PairInvite {
            host_id,
            name: nearby.map(|host| host.name.clone()).unwrap_or_default(),
            code,
            traverse: None,
            relay: None,
            addrs: nearby.map(|host| host.addrs.clone()).unwrap_or_default(),
        })
    }

    /// Reset for a fresh attempt and stamp it. Any in-flight browse or pair
    /// result from the previous generation is discarded when it lands.
    pub fn restart(&mut self) -> u64 {
        self.paired = None;
        self.invite = None;
        self.error = None;
        self.filled = false;
        self.busy = false;
        self.discovered.clear();
        self.browsing = true;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Apply a discovery result. Returns `false` when it belongs to a
    /// superseded attempt and was dropped.
    pub fn accept_browse(&mut self, generation: u64, hosts: Vec<DiscoveredHost>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.discovered = hosts;
        self.browsing = false;
        true
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

    /// Parse a `tcode://pair?…` invite. A fixed-origin client keeps its own
    /// origin and adopts only the code.
    pub fn fill_invite(&mut self, value: &str, window: &mut Window, cx: &mut App) -> bool {
        let Some(invite) = parse_pair_url(value) else {
            return false;
        };
        if self.fixed_endpoint.is_none() {
            self.address.update(cx, |state, cx| {
                state.set_value(invite.host_id.clone(), window, cx)
            });
        }
        self.code.update(cx, |state, cx| {
            state.set_value(invite.code.clone(), window, cx)
        });
        self.invite = Some(invite);
        self.filled = true;
        self.error = None;
        true
    }

    /// Fill a machine id and focus the connection code.
    pub fn fill_discovered(&mut self, host_id: String, window: &mut Window, cx: &mut App) {
        self.invite = None;
        self.error = None;
        self.address
            .update(cx, |state, cx| state.set_value(host_id, window, cx));
        self.code.update(cx, |state, cx| {
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
    use tcode_client::host::DiscoveredHost;

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

    fn discovered(name: &str) -> DiscoveredHost {
        DiscoveredHost {
            host_id: name.into(),
            name: name.into(),
            addrs: vec!["192.168.1.9:47420".into()],
        }
    }

    /// A pasted invite cannot retarget the browser away from its serving origin.
    #[gpui::test]
    fn a_fixed_origin_keeps_its_endpoint(cx: &mut TestAppContext) {
        let (form, cx) = cx.add_window_view(|window, cx| {
            Holder(PairForm::new(
                Some("https://app.example".into()),
                window,
                cx,
            ))
        });
        // Built by the producer the host actually uses, so the test cannot
        // drift from the invite format.
        let invite = tcode_client::pairing::pair_url(&PairInvite {
            host_id: "ab".repeat(32),
            name: "Host".into(),
            code: "123456".into(),
            traverse: None,
            relay: None,
            addrs: vec!["10.0.0.4:47420".into()],
        });
        cx.update(|window, cx| {
            form.update(cx, |holder, cx| {
                assert!(holder.0.fill_invite(&invite, window, cx), "valid invite");
            });
        });

        form.read_with(cx, |holder, cx| {
            let form = &holder.0;
            assert!(form.has_fixed_endpoint());
            assert!(!form.show_discovery(), "a fixed origin cannot browse");
            assert_eq!(form.address.read(cx).value(), "https://app.example");
            assert_eq!(form.code.read(cx).value(), "123456");
            let request = form.request(cx).expect("a complete request");
            assert_eq!(request.host_id, "", "a browser pairs with its own origin");
            assert_eq!(request.code, "123456");
        });
    }

    /// A scanned invite's addresses travel with the code only while the id
    /// in the field is the invite's; retyping another id drops them.
    #[gpui::test]
    fn the_code_goes_only_to_the_machine_id_in_the_field(cx: &mut TestAppContext) {
        let (form, cx) = cx.add_window_view(|window, cx| Holder(PairForm::new(None, window, cx)));
        let invite = PairInvite {
            host_id: "ab".repeat(32),
            name: "Host".into(),
            code: "123456".into(),
            traverse: None,
            relay: Some("https://relay.example/".into()),
            addrs: vec!["10.0.0.4:47420".into()],
        };
        let url = tcode_client::pairing::pair_url(&invite);
        cx.update(|window, cx| {
            form.update(cx, |holder, cx| {
                assert!(holder.0.fill_invite(&url, window, cx));
            });
        });
        form.read_with(cx, |holder, cx| {
            assert_eq!(holder.0.request(cx), Some(invite.clone()));
        });
        cx.update(|window, cx| {
            form.update(cx, |holder, cx| {
                holder.0.address.update(cx, |state, cx| {
                    state.set_value("cd".repeat(32), window, cx);
                });
                holder.0.code.update(cx, |state, cx| {
                    state.set_value("123456", window, cx);
                });
            });
        });
        form.read_with(cx, |holder, cx| {
            let request = holder.0.request(cx).unwrap();
            assert_eq!(request.host_id, "cd".repeat(32));
            assert!(request.addrs.is_empty());
            assert_eq!(request.relay, None);
        });
        cx.update(|window, cx| {
            form.update(cx, |holder, cx| {
                holder.0.address.update(cx, |state, cx| {
                    state.set_value("not an id", window, cx);
                });
            });
        });
        form.read_with(cx, |holder, cx| assert_eq!(holder.0.request(cx), None));
    }

    /// Browsing and pairing outlive the attempt that started them. A reply from
    /// a superseded attempt is dropped, so reopening the form cannot be
    /// retargeted by an answer the user has already moved on from.
    #[gpui::test]
    fn results_from_a_superseded_attempt_are_dropped(cx: &mut TestAppContext) {
        let (form, cx) = cx.add_window_view(|window, cx| Holder(PairForm::new(None, window, cx)));

        let stale = form.update(cx, |holder, _| holder.0.restart());
        let current = form.update(cx, |holder, _| holder.0.restart());
        assert_ne!(stale, current);

        form.update(cx, |holder, _| {
            assert!(
                !holder.0.accept_browse(stale, vec![discovered("stale")]),
                "a browse from the previous attempt must be dropped"
            );
            assert!(holder.0.discovered.is_empty());
            assert!(holder.0.browsing, "the current browse is still running");

            assert!(holder.0.accept_browse(current, vec![discovered("live")]));
            assert_eq!(holder.0.discovered.len(), 1);
            assert!(!holder.0.browsing);

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
