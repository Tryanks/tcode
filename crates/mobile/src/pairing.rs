//! The phone's pairing sheet.
//!
//! The rules (endpoint-bound fingerprints, stale-result generations, fixed
//! origins) live in [`tcode_ui::pairing::PairForm`] and are shared with the
//! desktop settings page; only this presentation is phone-specific.

use super::*;

/// Re-exported so `MobileRoot` keeps naming one type for its form state.
pub(super) use tcode_ui::pairing::PairForm;

impl MobileRoot {
    pub(super) fn open_pair(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generation = self.pair.restart();
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
                                this.pair.fill_invite(&value, window, cx);
                            }
                            cx.notify();
                        }
                    },
                ));
            }
        }
        let weak = cx.weak_entity();
        self.host.browse_hosts(
            Box::new(move |hosts, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if this.pair.accept_browse(generation, hosts) {
                        cx.notify();
                    }
                });
            }),
            cx,
        );
        self.sheet = Some(Sheet::Pair);
        cx.notify();
    }
    fn submit_pair(&mut self, cx: &mut Context<Self>) {
        if let Some(host) = self.pair.take_paired() {
            self.sheet = None;
            self.connect(host, cx);
            return;
        }
        let Some((request, generation)) = self.pair.begin_pair(cx) else {
            return;
        };
        let address = format!("{}:{}", request.addr, request.port);
        let weak = cx.weak_entity();
        self.host.pair(
            request,
            cx,
            Box::new(move |result, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if !matches!(this.sheet, Some(Sheet::Pair)) {
                        return;
                    }
                    if let Ok(host) = &result {
                        this.hosts.retain(|h| h.host_id != host.host_id);
                        this.hosts.push(host.clone());
                        this.host.save_hosts(&this.hosts);
                    }
                    if this.pair.finish_pair(generation, result, &address) {
                        cx.notify();
                    }
                });
            }),
        );
        cx.notify();
    }
    /// Discovery results prefill the endpoint and fingerprint, then focus the code field.
    fn nearby_hosts(&self, form: Div, cx: &mut Context<Self>) -> Div {
        if !self.pair.show_discovery() {
            return form;
        }
        let busy = self.pair.busy;
        // Group discovery results more tightly than the surrounding form fields.
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
                                .pin_discovered(addr.clone(), port, fp.clone(), window, cx);
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

    /// Paired, not yet connected: the pinned fingerprint next to the one
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

    /// Pairing content; `render_sheet` supplies its title and dismissal controls.
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
                                                Ok(value) => {
                                                    this.pair.fill_invite(&value, window, cx);
                                                }
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
        if !self.pair.has_fixed_endpoint() {
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
