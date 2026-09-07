//! The client half of pairing, shared by every shell.
//!
//! Only the presentation differs between the desktop settings page and the
//! phone sheet; the rules do not, and they are the parts that are easy to get
//! subtly wrong:
//!
//! - **A pinned fingerprint is bound to the endpoint it came from.** An invite
//!   or a discovery result pins a certificate for one `addr:port`. Edit either
//!   field and the pin no longer applies, so the request goes out unpinned
//!   rather than pinning a stranger's certificate to the typed address.
//! - **Results are generation-stamped.** Browsing and pairing are slow and the
//!   user can reopen or retarget the form while they run; a reply from a
//!   superseded attempt is dropped instead of overwriting the current state.
//! - **A fixed origin is not editable.** A browser can only pair with the origin
//!   that served it, so where `ClientHost::fixed_pairing_endpoint` answers, the
//!   address and port are fixed, discovery is pointless and both are hidden.

use gpui::{App, AppContext as _, Entity, Window};
use tcode_client::host::{DiscoveredHost, PairRequest};
use tcode_client::pairing::{PairedHost, is_pairing_code, parse_pair_url};

use crate::widgets::input::InputState;

/// Default port a tcode host listens on.
pub const DEFAULT_REMOTE_PORT: u16 = 47_420;

pub struct PairForm {
    /// The certificate fingerprint pinned for `pin_endpoint`, if any.
    pub fingerprint: String,
    pin_endpoint: Option<(String, u16)>,
    fixed_endpoint: Option<(String, u16)>,
    pub discovered: Vec<DiscoveredHost>,
    pub browsing: bool,
    /// Paired but not yet connected: the shell shows the fingerprint for
    /// comparison before any traffic flows.
    pub paired: Option<PairedHost>,
    pub address: Entity<InputState>,
    pub port: Entity<InputState>,
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
    /// `Some` pins the address and port and hides both fields.
    pub fn new(fixed: Option<(String, u16)>, window: &mut Window, cx: &mut App) -> Self {
        Self {
            fingerprint: String::new(),
            pin_endpoint: None,
            discovered: Vec::new(),
            browsing: false,
            paired: None,
            address: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::tr!("hosts.pair.address_placeholder").into_owned())
                    .default_value(
                        fixed
                            .as_ref()
                            .map(|(addr, _)| addr.clone())
                            .unwrap_or_default(),
                    )
            }),
            port: cx.new(|cx| {
                InputState::new(window, cx).default_value(
                    fixed
                        .as_ref()
                        .map(|(_, port)| *port)
                        .unwrap_or(DEFAULT_REMOTE_PORT)
                        .to_string(),
                )
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

    /// The submittable request, or `None` while the form is incomplete. The
    /// pinned fingerprint travels only when the endpoint still matches the one
    /// it was pinned for.
    pub fn request(&self, cx: &App) -> Option<PairRequest> {
        let addr = self.address.read(cx).value().trim().to_owned();
        let port = self
            .port
            .read(cx)
            .value()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)?;
        let code = self.code.read(cx).value().to_string();
        if addr.is_empty() || addr.contains(char::is_whitespace) || !is_pairing_code(&code) {
            return None;
        }
        Some(PairRequest {
            fingerprint: if self
                .pin_endpoint
                .as_ref()
                .is_some_and(|(pinned, pinned_port)| pinned == &addr && *pinned_port == port)
            {
                self.fingerprint.clone()
            } else {
                String::new()
            },
            addr,
            port,
            code,
        })
    }

    /// Reset for a fresh attempt and stamp it. Any in-flight browse or pair
    /// result from the previous generation is discarded when it lands.
    pub fn restart(&mut self) -> u64 {
        self.paired = None;
        self.fingerprint.clear();
        self.pin_endpoint = None;
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
    pub fn begin_pair(&mut self, cx: &App) -> Option<(PairRequest, u64)> {
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
                self.fingerprint = host.fingerprint.clone();
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
    /// endpoint and adopts only the code and the pin.
    pub fn fill_invite(&mut self, value: &str, window: &mut Window, cx: &mut App) -> bool {
        let Some(invite) = parse_pair_url(value) else {
            return false;
        };
        let Some(addr) = invite.addrs.first().cloned() else {
            return false;
        };
        self.fingerprint = invite.fp;
        self.pin_endpoint = Some((addr.clone(), invite.port));
        if self.fixed_endpoint.is_none() {
            self.address
                .update(cx, |state, cx| state.set_value(addr, window, cx));
            self.port.update(cx, |state, cx| {
                state.set_value(invite.port.to_string(), window, cx)
            });
        }
        self.code
            .update(cx, |state, cx| state.set_value(invite.code, window, cx));
        self.filled = true;
        self.error = None;
        true
    }

    /// Adopt a discovered host: fill the endpoint, pin its fingerprint to that
    /// endpoint and clear the code so only the six digits remain to type.
    pub fn pin_discovered(
        &mut self,
        addr: String,
        port: u16,
        fingerprint: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.pin_endpoint = Some((addr.clone(), port));
        self.fingerprint = fingerprint;
        self.error = None;
        self.address
            .update(cx, |state, cx| state.set_value(addr, window, cx));
        self.port.update(cx, |state, cx| {
            state.set_value(port.to_string(), window, cx)
        });
        self.code.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
    }
}

/// Turn a transport failure into something the user can act on: a wrong or
/// expired code, an unreachable endpoint, or the raw reason.
pub fn pair_error(error: &str, address: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("403")
        || lower.contains("401")
        || lower.contains("expired")
        || lower.contains("invalid code")
    {
        crate::tr!("hosts.pair.bad_code").into_owned()
    } else if ["timeout", "timed out", "refused", "connect", "dns"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        crate::tr!("hosts.pair.network_error", address = address).into_owned()
    } else {
        crate::tr!("hosts.pair.failed", reason = error).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use tcode_client::host::DiscoveredHost;

    use super::*;

    fn discovered(name: &str) -> DiscoveredHost {
        DiscoveredHost {
            host_id: name.into(),
            name: name.into(),
            addr: "192.168.1.9".into(),
            port: 47_420,
            fp: "ab".repeat(32),
        }
    }

    /// A browser can only reach the origin that served it. An invite pasted
    /// there contributes its code and its pin, but must not silently retarget
    /// the connection at the address printed in the invite — and because the
    /// endpoint then differs from the pinned one, the request goes out unpinned
    /// rather than pinning a stranger's certificate to this origin.
    #[gpui::test]
    fn a_fixed_origin_keeps_its_endpoint_and_drops_a_mismatched_pin(cx: &mut TestAppContext) {
        let (form, cx) = cx.add_window_view(|window, cx| {
            Holder(PairForm::new(Some(("app.example".into(), 443)), window, cx))
        });
        // Built by the producer the host actually uses, so the test cannot
        // drift from the invite format.
        let invite = tcode_client::pairing::pair_url(&tcode_client::pairing::PairInvite {
            host_id: "h".into(),
            name: "Host".into(),
            addrs: vec!["10.0.0.4".into()],
            port: 47_420,
            code: "123456".into(),
            fp: "cd".repeat(32),
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
            assert_eq!(form.address.read(cx).value(), "app.example");
            assert_eq!(form.port.read(cx).value(), "443");
            assert_eq!(form.code.read(cx).value(), "123456");
            let request = form.request(cx).expect("a complete request");
            assert_eq!((request.addr.as_str(), request.port), ("app.example", 443));
            assert_eq!(
                request.fingerprint, "",
                "a pin for another endpoint must not travel with this one"
            );
        });
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
                "a rejected code must read as a code problem, not a network one"
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
