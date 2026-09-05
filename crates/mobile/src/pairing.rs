use super::*;
use host::{PairRequest, is_pairing_code, parse_pair_url};

pub(super) struct PairForm {
    pub fingerprint: String,
    pin_endpoint: Option<(String, u16)>,
    discovered: Vec<host::DiscoveredHost>,
    browsing: bool,
    paired: Option<host::PairedHost>,
    pub address: Entity<InputState>,
    pub port: Entity<InputState>,
    pub code: Entity<InputState>,
    pub busy: bool,
    pub error: Option<String>,
    pub filled: bool,
    pub generation: u64,
    listening: bool,
}
impl PairForm {
    pub fn new(host: &SharedHost, window: &mut Window, cx: &mut Context<MobileRoot>) -> Self {
        let fixed = host.fixed_pairing_endpoint();
        Self {
            fingerprint: String::new(),
            pin_endpoint: None,
            discovered: Vec::new(),
            browsing: false,
            paired: None,
            address: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(label("address_placeholder"))
                    .default_value(fixed.as_ref().map(|v| v.0.clone()).unwrap_or_default())
            }),
            port: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(fixed.map(|v| v.1).unwrap_or(47420).to_string())
            }),
            code: cx.new(|cx| InputState::new(window, cx).placeholder(label("code_placeholder"))),
            busy: false,
            error: None,
            filled: false,
            generation: 0,
            listening: false,
        }
    }
    fn request(&self, cx: &App) -> Option<PairRequest> {
        let addr = self.address.read(cx).value().trim().to_owned();
        let port = self
            .port
            .read(cx)
            .value()
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)?;
        let code = self.code.read(cx).value().to_string();
        if addr.is_empty() || addr.contains(char::is_whitespace) || !is_pairing_code(&code) {
            return None;
        }
        Some(PairRequest {
            addr: addr.clone(),
            port,
            code,
            fingerprint: if self
                .pin_endpoint
                .as_ref()
                .is_some_and(|(a, p)| a == &addr && *p == port)
            {
                self.fingerprint.clone()
            } else {
                String::new()
            },
        })
    }
}
impl MobileRoot {
    pub(super) fn open_pair(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pair.paired = None;
        self.pair.fingerprint.clear();
        self.pair.pin_endpoint = None;
        self.pair.error = None;
        self.pair.filled = false;
        self.pair.busy = false;
        self.pair.generation += 1;
        // Install once: paste events from all three fields use the same parser.
        if !self.pair.listening {
            self.pair.listening = true;
            for field in [&self.pair.address, &self.pair.port, &self.pair.code] {
                self.subscriptions.push(cx.subscribe_in(
                    field,
                    window,
                    |this, input, event, window, cx| {
                        if matches!(event, InputEvent::Change) {
                            let value = input.read(cx).value().to_string();
                            if value.trim().starts_with("tcode://pair?") {
                                this.fill_invite(&value, window, cx);
                            }
                            cx.notify();
                        }
                    },
                ));
            }
        }
        self.pair.discovered.clear();
        self.pair.browsing = true;
        let weak = cx.weak_entity();
        let generation = self.pair.generation;
        self.host.browse_hosts(
            Box::new(move |hosts, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if this.pair.generation == generation {
                        this.pair.discovered = hosts;
                        this.pair.browsing = false;
                        cx.notify();
                    }
                });
            }),
            cx,
        );
        self.sheet = Some(Sheet::Pair);
        cx.notify();
    }
    fn fill_invite(&mut self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(invite) = parse_pair_url(value) {
            self.pair.fingerprint = invite.fp;
            self.pair.pin_endpoint = Some((invite.addrs[0].clone(), invite.port));
            if self.host.fixed_pairing_endpoint().is_none() {
                self.pair
                    .address
                    .update(cx, |s, cx| s.set_value(invite.addrs[0].clone(), window, cx));
                self.pair
                    .port
                    .update(cx, |s, cx| s.set_value(invite.port.to_string(), window, cx));
            }
            self.pair
                .code
                .update(cx, |s, cx| s.set_value(invite.code, window, cx));
            self.pair.filled = true;
            self.pair.error = None;
        }
    }
    fn submit_pair(&mut self, cx: &mut Context<Self>) {
        if let Some(host) = self.pair.paired.take() {
            self.sheet = None;
            self.connect(host, cx);
            return;
        }
        if self.pair.busy {
            return;
        }
        let Some(request) = self.pair.request(cx) else {
            return;
        };
        self.pair.busy = true;
        self.pair.error = None;
        let address = format!("{}:{}", request.addr, request.port);
        let generation = self.pair.generation;
        let weak = cx.weak_entity();
        self.host.pair(
            request,
            cx,
            Box::new(move |result, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if generation != this.pair.generation
                        || !matches!(this.sheet, Some(Sheet::Pair))
                    {
                        return;
                    }
                    this.pair.busy = false;
                    match result {
                        Ok(host) => {
                            this.hosts.retain(|h| h.host_id != host.host_id);
                            this.hosts.push(host.clone());
                            this.host.save_hosts(&this.hosts);
                            this.pair.fingerprint = host.fingerprint.clone();
                            this.pair.paired = Some(host);
                        }
                        Err(error) => this.pair.error = Some(pair_error(&error, &address)),
                    }
                    cx.notify();
                });
            }),
        );
        cx.notify();
    }
    /// "Nearby hosts" (§3.2): what DNS-SD found, at most three rows. Tapping a
    /// row fills the endpoint and its advertised fingerprint and drops the
    /// caret in the code field, which is all that is left to type.
    fn nearby_hosts(&self, form: Div, cx: &mut Context<Self>) -> Div {
        if self.host.fixed_pairing_endpoint().is_some()
            || (!self.pair.browsing && self.pair.discovered.is_empty())
        {
            return form;
        }
        let busy = self.pair.busy;
        // The heading and its rows are one group at 8, not three form fields
        // at 16 (§3.2).
        let mut nearby = v_flex().gap(px(8.)).child(
            h_flex()
                .gap(px(6.))
                .items_center()
                .text_color(cx.theme().muted_foreground)
                .child(text(label("nearby_hosts"), 13.).line_height(px(18.)))
                .when(self.pair.browsing, |row| {
                    row.child(spinner(12., cx.theme().muted_foreground))
                }),
        );
        for (index, found) in self.pair.discovered.iter().take(3).enumerate() {
            let (addr, port, fp) = (found.addr.clone(), found.port, found.fp.clone());
            let endpoint = format!("{addr}:{port}");
            nearby = nearby.child(
                material::accessible_clickable(
                    material::group(cx),
                    ("nearby", index),
                    Role::Button,
                    found.name.clone(),
                    cx,
                )
                .min_h(px(56.))
                .px(px(14.))
                .py(px(10.))
                .gap(px(2.))
                .justify_center()
                .when(busy, |row| row.opacity(0.5))
                .when(!busy, |row| {
                    row.cursor_pointer()
                        .active(|s| s.bg(cx.theme().foreground.opacity(0.08)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.pair
                                .address
                                .update(cx, |s, cx| s.set_value(addr.clone(), window, cx));
                            this.pair
                                .port
                                .update(cx, |s, cx| s.set_value(port.to_string(), window, cx));
                            this.pair.fingerprint = fp.clone();
                            this.pair.error = None;
                            this.pair.code.update(cx, |s, cx| {
                                s.set_value("", window, cx);
                                s.focus(window, cx);
                            });
                            cx.notify();
                        }))
                })
                .child(text(found.name.clone(), 15.).font_semibold().truncate())
                .child(
                    text(endpoint, 13.)
                        .line_height(px(18.))
                        .text_color(cx.theme().muted_foreground)
                        .truncate(),
                ),
            );
        }
        form.child(nearby)
    }

    /// Paired, not yet connected (§3.2): the pinned fingerprint next to the one
    /// the host shows, so a swapped certificate is caught before any traffic.
    fn render_pair_confirm(&self, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap(px(16.))
            .child(
                material::group(cx).p(px(14.)).child(
                    text(
                        tcode_client::pairing::display_fingerprint(&self.pair.fingerprint),
                        14.,
                    )
                    .line_height(px(20.))
                    .font_family(cx.theme().mono_font_family.clone()),
                ),
            )
            .child(
                text(label("fingerprint_compare"), 13.)
                    .line_height(px(18.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                button("pair-connect", label("connect_host"), true, true, cx)
                    .h(px(50.))
                    .w_full()
                    .on_click(cx.listener(|this, _, _, cx| this.submit_pair(cx))),
            )
    }

    /// The pairing sheet's body (§3.2). The sheet chrome — grabber, title,
    /// Cancel — belongs to `render_sheet`; this is scan, form, error, submit.
    pub(super) fn render_pair(&mut self, cx: &mut Context<Self>) -> Div {
        if self.pair.paired.is_some() {
            return self.render_pair_confirm(cx);
        }
        let busy = self.pair.busy;
        let mut form = v_flex().gap(px(16.));
        if self.host.supports_qr() {
            form = form
                .child(
                    button("scan", label("scan"), false, !busy, cx)
                        .w_full()
                        .h(px(50.))
                        .border_1()
                        .border_color(cx.theme().primary)
                        .text_color(cx.theme().primary)
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.pair.busy {
                                return;
                            }
                            let weak = cx.weak_entity();
                            let handle = window.window_handle();
                            this.host.scan_qr(
                                Box::new(move |result, cx| {
                                    let _ = handle.update(cx, |_, window, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            match result {
                                                Ok(value) => this.fill_invite(&value, window, cx),
                                                Err(error) => {
                                                    this.pair.error = Some(
                                                        tr!("mobile.pair_error", reason = error)
                                                            .into_owned(),
                                                    )
                                                }
                                            };
                                            cx.notify();
                                        });
                                    });
                                }),
                                cx,
                            );
                        })),
                )
                .child(
                    text(label("manual"), 13.)
                        .text_center()
                        .text_color(cx.theme().muted_foreground),
                );
        }
        form = self.nearby_hosts(form, cx);
        if let Some(error) = &self.pair.error {
            form = form.child(
                text(error.clone(), 13.)
                    .line_height(px(18.))
                    .text_color(cx.theme().danger_foreground),
            );
        }
        if self.host.fixed_pairing_endpoint().is_none() {
            form = form
                .child(field("address", &self.pair.address, busy, false, cx))
                .child(field("port", &self.pair.port, busy, false, cx));
        }
        form = form
            .child(field("code", &self.pair.code, busy, true, cx))
            .child(
                text(label("pair_help"), 13.)
                    .line_height(px(18.))
                    .text_color(cx.theme().muted_foreground),
            );
        if self.pair.filled {
            form = form.child(
                text(label("pair_filled"), 13.)
                    .line_height(px(18.))
                    .text_color(cx.theme().muted_foreground),
            );
        }
        if !self.pair.fingerprint.is_empty() {
            form = form.child(
                text(fingerprint_line(&self.pair.fingerprint), 13.)
                    .line_height(px(18.))
                    .text_color(cx.theme().muted_foreground),
            );
        }
        form.child(
            button(
                "pair-submit",
                label(if busy { "pairing" } else { "pair" }),
                true,
                !busy && self.pair.request(cx).is_some(),
                cx,
            )
            .h(px(50.))
            .w_full()
            .on_click(cx.listener(|this, _, _, cx| this.submit_pair(cx))),
        )
    }
}
/// "Fingerprint: a1b2 c3d4 …" for whatever the invite or discovery pinned.
fn fingerprint_line(fingerprint: &str) -> String {
    tr!(
        "mobile.fingerprint",
        fingerprint = tcode_client::pairing::display_fingerprint(fingerprint)
    )
    .into_owned()
}
fn field(title: &str, state: &Entity<InputState>, busy: bool, code: bool, cx: &App) -> Div {
    v_flex()
        .gap(px(6.))
        .child(text(label(title), 13.).text_color(cx.theme().muted_foreground))
        .child(
            Input::new(state)
                .disabled(busy)
                .h(px(48.))
                .min_h(px(48.))
                .max_h(px(48.))
                .rounded(px(12.))
                .bg(cx.theme().secondary)
                .text_size(px(if code { 24. } else { 16. }))
                .when(code, |input| {
                    input.font_family(cx.theme().mono_font_family.clone())
                }),
        )
}

fn pair_error(error: &str, address: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("403")
        || lower.contains("401")
        || lower.contains("expired")
        || lower.contains("invalid code")
    {
        label("pair_code_error")
    } else if ["timeout", "timed out", "refused", "connect", "dns"]
        .iter()
        .any(|s| lower.contains(s))
    {
        tr!("mobile.pair_network_error", address = address).into_owned()
    } else {
        tr!("mobile.pair_error", reason = error).into_owned()
    }
}
#[cfg(test)]
mod tests {
    use super::{label, pair_error};
    use tcode_ui::tr;
    #[test]
    fn pairing_failures_keep_actionable_categories() {
        assert_eq!(
            pair_error("HTTP 403 Forbidden", "a:1"),
            label("pair_code_error")
        );
        assert_eq!(
            pair_error("Connection refused", "a:1"),
            tr!("mobile.pair_network_error", address = "a:1").into_owned()
        );
        assert_eq!(
            pair_error("bad response", "a:1"),
            tr!("mobile.pair_error", reason = "bad response").into_owned()
        );
    }
}
