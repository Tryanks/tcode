//! Shared origin form; generation stamps discard superseded pairing results.
use gpui::{App, AppContext as _, Entity, Window};
use tcode_client::host::{DiscoveredHost, PairRequest};
use tcode_client::pairing::{PairedHost, is_pairing_code, parse_origin, parse_pair_url};

use crate::widgets::input::InputState;

/// Default port a tcode host listens on.
pub use tcode_client::pairing::DEFAULT_REMOTE_PORT;

pub struct PairForm {
    fixed_endpoint: Option<String>,
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

    /// Validate the origin and six-digit code before submission.
    pub fn request(&self, cx: &App) -> Option<PairRequest> {
        let origin = parse_origin(
            self.fixed_endpoint
                .as_deref()
                .unwrap_or(self.address.read(cx).value().as_ref()),
        )
        .ok()?;
        let code = self.code.read(cx).value().to_string();
        if !is_pairing_code(&code) {
            return None;
        }
        Some(PairRequest { origin, code })
    }

    /// Reset for a fresh attempt and stamp it. Any in-flight browse or pair
    /// result from the previous generation is discarded when it lands.
    pub fn restart(&mut self) -> u64 {
        self.paired = None;
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
            self.address
                .update(cx, |state, cx| state.set_value(invite.origin, window, cx));
        }
        self.code
            .update(cx, |state, cx| state.set_value(invite.code, window, cx));
        self.filled = true;
        self.error = None;
        true
    }

    /// Fill a discovered origin and focus the connection code.
    pub fn fill_discovered(&mut self, origin: String, window: &mut Window, cx: &mut App) {
        self.error = None;
        self.address
            .update(cx, |state, cx| state.set_value(origin, window, cx));
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
            origin: "http://192.168.1.9:47420".into(),
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
        let invite = tcode_client::pairing::pair_url(&tcode_client::pairing::PairInvite {
            host_id: "h".into(),
            name: "Host".into(),
            origin: "http://10.0.0.4:47420".into(),
            code: "123456".into(),
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
            assert_eq!(request.origin, "https://app.example");
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
