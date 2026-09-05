//! The phone's shell: nav bar, the three pages, and the bottom sheets
//! (docs/mobile-design.md §3).

use super::*;

/// The nav bar (§3.0): 52pt on paper, a centered title column, a chevron back
/// button carrying the parent page's title, at most two 44×44 icon buttons, and
/// a faded hairline underneath.
fn nav_bar(
    back: Option<AnyElement>,
    title: SharedString,
    subtitle: Option<AnyElement>,
    actions: Vec<AnyElement>,
    cx: &App,
) -> Div {
    debug_assert!(actions.len() <= 2, "§3.0 allows at most two nav actions");
    v_flex()
        .flex_none()
        .w_full()
        .bg(material::content_surface(cx))
        .child(
            div()
                .relative()
                .w_full()
                .h(px(52.))
                .child(
                    // Centered on the bar itself, not between the buttons, so
                    // the title does not drift with the back button's width.
                    v_flex()
                        .absolute()
                        .inset_0()
                        .px(px(96.))
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .max_w_full()
                                .min_w_0()
                                .truncate()
                                .text_size(px(17.))
                                .line_height(px(22.))
                                .font_semibold()
                                .child(title),
                        )
                        .children(subtitle),
                )
                .child(
                    h_flex()
                        .absolute()
                        .inset_0()
                        .px(px(4.))
                        .items_center()
                        .children(back)
                        .child(div().flex_1())
                        .children(actions),
                ),
        )
        .child(material::faded_hairline(cx))
}

/// The back control (§3.0): a 20pt chevron plus the parent page's title, both
/// in `foreground` — never the primary blue.
fn back_button(id: &'static str, parent: SharedString, cx: &App) -> Stateful<Div> {
    material::accessible_clickable(h_flex(), id, Role::Button, parent.clone(), cx)
        .flex_none()
        .h(px(44.))
        .pl(px(4.))
        .pr(px(10.))
        .gap(px(2.))
        .items_center()
        .rounded(px(12.))
        .cursor_pointer()
        .text_color(cx.theme().foreground)
        .active(|s| s.bg(cx.theme().foreground.opacity(0.08)))
        .child(
            Icon::empty()
                .path("icons/chevron-left.svg")
                .size(px(20.))
                .flex_none(),
        )
        .child(
            div()
                .max_w(px(96.))
                .min_w_0()
                .truncate()
                .text_size(px(15.))
                .line_height(px(20.))
                .child(parent),
        )
}

impl MobileRoot {
    // -- §3.1 hosts --------------------------------------------------------

    pub(super) fn render_hosts(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let last = self.host.last_host_id();
        let mut list = v_flex().gap(px(12.)).px(px(16.)).pb(px(16.));
        for (index, host) in self.hosts.iter().enumerate() {
            list = list.child(self.render_host_card(index, host, last.as_deref(), cx));
        }

        let body = if self.hosts.is_empty() {
            material::empty_state(
                Icon::empty().path("icons/monitor-smartphone.svg"),
                label("hosts_empty"),
                label("hosts_help"),
                cx,
            )
            .into_any_element()
        } else {
            scroll("host-list", list).into_any_element()
        };

        v_flex()
            .size_full()
            .bg(material::content_surface(cx))
            .child(
                v_flex()
                    .flex_none()
                    .px(px(16.))
                    .pt(px(20.))
                    .pb(px(16.))
                    .gap(px(6.))
                    // The wordmark over the large title is the only place the
                    // brand appears outside About (§3.0).
                    .child(div().flex_none().child(material::brand_wordmark(cx)))
                    .child(
                        text(label("hosts"), 28.)
                            .line_height(px(34.))
                            .font_semibold(),
                    )
                    .child(text(label("hosts_intro"), 15.).text_color(cx.theme().muted_foreground)),
            )
            .child(body)
            .child(
                div().flex_none().px(px(16.)).pb(px(16.)).child(
                    button("pair-new", label("pair_new"), true, true, cx)
                        .w_full()
                        .h(px(50.))
                        .on_click(cx.listener(|this, _, window, cx| this.open_pair(window, cx))),
                ),
            )
            .into_any_element()
    }

    /// One host card (§3.1): a T2 group carrying the name, its address and last
    /// connection, and — once the host is used — a "Last used" chip. The whole
    /// card is the tap target, so it carries no chevron.
    fn render_host_card(
        &self,
        index: usize,
        host: &PairedHost,
        last: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let connect = host.clone();
        let remove = host.clone();
        let long_host = host.clone();
        let entity = cx.entity();
        let long_press = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                window.on_mouse_event(move |event: &LongPressEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble
                        && event.phase == TouchPhase::Started
                        && bounds.contains(&event.position)
                    {
                        window.capture_long_press(&entity);
                        window.prevent_default();
                        entity.update(cx, |this, cx| {
                            this.sheet = Some(Sheet::Remove(long_host.clone()));
                            cx.notify();
                        });
                    }
                });
            },
        )
        .absolute()
        .size_full();
        let when = host
            .last_connected_unix
            .map(|at| {
                tr!(
                    "mobile.connected_ago",
                    time = tcode_ui::time::humanize_ago(now_secs().saturating_sub(at))
                )
                .into_owned()
            })
            .unwrap_or_else(|| label("never_connected"));

        material::accessible_clickable(
            material::group(cx),
            ("host", index),
            Role::Button,
            host.name.clone(),
            cx,
        )
        .relative()
        .child(long_press)
        .min_h(px(72.))
        .p(px(14.))
        .gap(px(4.))
        .cursor_pointer()
        .active(|s| s.bg(cx.theme().foreground.opacity(0.08)))
        .child(
            h_flex()
                .gap(px(8.))
                .items_center()
                .child(
                    text(host.name.clone(), 17.)
                        .font_semibold()
                        .flex_1()
                        .min_w_0()
                        .truncate(),
                )
                .when(last == Some(host.host_id.as_str()), |el| {
                    el.child(material::semantic_chip(
                        label("last_host"),
                        cx.theme().muted,
                        cx.theme().muted_foreground,
                    ))
                }),
        )
        .child(
            text(
                format!(
                    "{}:{} · {}",
                    host.addrs.first().cloned().unwrap_or_default(),
                    host.port,
                    when
                ),
                14.,
            )
            .text_color(cx.theme().muted_foreground)
            .truncate(),
        )
        // certificate fingerprint (P4c): a third 12pt mono line goes here once
        // `PairedHost` carries the pinned fingerprint.
        .on_click(cx.listener(move |this, _, _, cx| this.connect(connect.clone(), cx)))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _, _, cx| {
                this.sheet = Some(Sheet::Remove(remove.clone()));
                cx.notify();
            }),
        )
        .into_any_element()
    }

    // -- §4 connection status ---------------------------------------------

    /// Caption, color, and whether the glyph spins, for the current link (§4).
    fn connection(&self, cx: &App) -> (String, Hsla, bool) {
        if self.offline() {
            (label("offline"), cx.theme().danger_foreground, false)
        } else if !self.index_ready {
            (label("connecting"), cx.theme().muted_foreground, true)
        } else {
            match self.state {
                ConnectionState::Connected => {
                    (label("connected"), cx.theme().muted_foreground, false)
                }
                ConnectionState::Reconnecting { attempt } => (
                    tr!("mobile.reconnecting", attempt = attempt).into_owned(),
                    cx.theme().warning_foreground,
                    true,
                ),
                ConnectionState::Offline => (label("offline"), cx.theme().danger_foreground, false),
            }
        }
    }

    fn connection_badge(&self, cx: &App) -> Div {
        let (caption, color, spinning) = self.connection(cx);
        h_flex()
            .gap(px(4.))
            .items_center()
            .text_color(color)
            .child(if spinning {
                spinner(12., color).into_any_element()
            } else {
                div()
                    .size(px(6.))
                    .flex_none()
                    .rounded_full()
                    .bg(if self.online() {
                        cx.theme().success
                    } else {
                        color
                    })
                    .into_any_element()
            })
            .child(text(caption, 13.).line_height(px(18.)).truncate())
    }

    // -- §3.3 thread list --------------------------------------------------

    pub(super) fn render_threads(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let back = back_button("back-hosts", label("hosts").into(), cx)
            .on_click(cx.listener(|this, _, _, cx| {
                this.back(cx);
            }))
            .into_any_element();
        let actions = vec![
            icon_button(
                "new-thread",
                label("new_thread"),
                IconName::Plus,
                self.online(),
                cx,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if !this.online() {
                    return;
                }
                let projects = this
                    .store
                    .as_ref()
                    .map(|store| store.read(cx).projects())
                    .unwrap_or_default();
                if projects.len() == 1 {
                    this.start_draft(projects[0].clone(), window, cx);
                } else {
                    this.sheet = Some(Sheet::Projects);
                    cx.notify();
                }
            }))
            .into_any_element(),
            icon_button("settings", label("settings"), IconName::Settings, true, cx)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.sheet = Some(Sheet::Settings);
                    cx.notify();
                }))
                .into_any_element(),
        ];
        v_flex()
            .size_full()
            .bg(material::content_surface(cx))
            .child(nav_bar(
                Some(back),
                self.connected_host
                    .as_ref()
                    .map(|host| SharedString::from(host.name.clone()))
                    .unwrap_or_default(),
                Some(self.connection_badge(cx).into_any_element()),
                actions,
                cx,
            ))
            .child(div().flex_1().min_h_0().children(self.sidebar.clone()))
            .into_any_element()
    }

    // -- §3.4 thread -------------------------------------------------------

    pub(super) fn render_thread(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let meta = self
            .store
            .as_ref()
            .and_then(|store| store.read(cx).chat_active_session());
        let project = meta
            .as_ref()
            .and_then(|(_, cwd, _)| {
                self.store.as_ref().and_then(|store| {
                    store
                        .read(cx)
                        .projects()
                        .into_iter()
                        .find(|project| project.root == *cwd)
                })
            })
            .map(|project| project.name)
            .unwrap_or_default();
        let title = meta
            .map(|(title, _, draft)| if draft { label("new_thread") } else { title })
            .unwrap_or_else(|| label("new_thread"));
        let back = back_button("back-threads", label("threads").into(), cx)
            .on_click(cx.listener(|this, _, _, cx| {
                this.back(cx);
            }))
            .into_any_element();
        // The connection only earns space here when it is not "Connected" (§4).
        let actions = if self.online() {
            vec![]
        } else {
            let (_, color, _) = self.connection(cx);
            vec![
                self.connection_badge(cx)
                    .flex_none()
                    .px(px(8.))
                    .h(px(24.))
                    .max_w(px(150.))
                    .rounded_full()
                    .bg(color.opacity(0.12))
                    .into_any_element(),
            ]
        };
        v_flex()
            .size_full()
            .bg(material::content_surface(cx))
            .child(nav_bar(
                Some(back),
                title.into(),
                (!project.is_empty()).then(|| {
                    text(project, 13.)
                        .line_height(px(18.))
                        .max_w_full()
                        .min_w_0()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .into_any_element()
                }),
                actions,
                cx,
            ))
            .child(div().flex_1().min_h_0().children(self.chat.clone()))
            .into_any_element()
    }

    // -- §3.0 / §3.2 / §3.6 sheets ----------------------------------------

    /// The bottom sheet: T3 body, grabber, title bar, and a 180ms slide-up with
    /// the backdrop fading in step (§3.0). It stays mounted through its exit so
    /// dismissal animates as well.
    pub(super) fn render_sheet(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let open = self.sheet.is_some();
        if open {
            self.mounted_sheet = self.sheet.clone();
        }
        let sheet = self.mounted_sheet.clone()?;
        let sample = Presence::new("mobile-sheet", open)
            .transition(Transition::new(Duration::from_millis(180)))
            .sample(window, cx);
        if !sample.should_render() {
            self.mounted_sheet = None;
            return None;
        }
        let progress = sample.progress;

        let title = match &sheet {
            Sheet::Pair => label("pair_title"),
            Sheet::Remove(host) => tr!("mobile.remove_host", name = host.name).into_owned(),
            Sheet::Projects => label("choose_project"),
            Sheet::Settings => label("settings"),
        };
        let content = match sheet {
            Sheet::Pair => self.render_pair(cx),
            Sheet::Remove(host) => self.render_remove(host, cx),
            Sheet::Projects => self.render_projects(cx),
            Sheet::Settings => self.render_settings(cx),
        };
        let safe_bottom = self.host.safe_area().bottom;
        let max_height =
            (window.viewport_size().height - px(70.) - self.host.safe_area().top - safe_bottom)
                .max(px(180.));

        Some(
            div()
                .absolute()
                .inset_0()
                .overflow_hidden()
                .bg(material::scrim(progress, cx))
                .flex()
                .flex_col()
                .justify_end()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.pair.generation += 1;
                        this.sheet = None;
                        cx.notify();
                    }),
                )
                .child(
                    v_flex()
                        .id("mobile-sheet")
                        .occlude()
                        .max_h(max_height)
                        .w_full()
                        // A negative bottom margin slides the sheet down out of
                        // the clipped backdrop; at rest it is zero.
                        .mb(px(0.) - max_height * (1. - progress))
                        .rounded_t(material::radius_overlay_sheet())
                        .bg(cx.theme().popover)
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .shadow_xl()
                        .child(material::sheet_grabber(cx))
                        .child(
                            h_flex()
                                .flex_none()
                                .h(px(48.))
                                .px(px(16.))
                                .gap(px(8.))
                                .items_center()
                                .child(
                                    text(title, 17.)
                                        .font_semibold()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate(),
                                )
                                .child(
                                    button("sheet-cancel", label("cancel"), false, true, cx)
                                        .text_size(px(15.))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.pair.generation += 1;
                                            this.sheet = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(material::faded_hairline(cx))
                        .child(
                            div()
                                .id("sheet-scroll")
                                .min_h_0()
                                .overflow_y_scroll()
                                .child(content.flex_none().p(px(16.)).pb(px(16.) + safe_bottom)),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_remove(&self, host: PairedHost, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap(px(16.))
            .child(text(label("remove_help"), 15.).text_color(cx.theme().muted_foreground))
            .child(
                h_flex()
                    .gap(px(12.))
                    .child(
                        button("remove-cancel", label("cancel"), false, true, cx)
                            .flex_1()
                            .h(px(50.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sheet = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        button("remove-confirm", label("remove"), false, true, cx)
                            .text_color(cx.theme().danger_foreground)
                            .flex_1()
                            .h(px(50.))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.hosts.retain(|h| h.host_id != host.host_id);
                                this.host.save_hosts(&this.hosts);
                                if this.host.last_host_id().as_deref() == Some(&host.host_id) {
                                    this.host.set_last_host_id(None);
                                }
                                this.sheet = None;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_projects(&self, cx: &mut Context<Self>) -> Div {
        let mut body = v_flex().gap(px(8.));
        if let Some(store) = &self.store {
            for project in store.read(cx).projects() {
                body = body.child(
                    button(
                        SharedString::from(format!("project-{}", project.id)),
                        project.name.clone(),
                        false,
                        self.online(),
                        cx,
                    )
                    .h(px(52.))
                    .w_full()
                    .justify_start()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_draft(project.clone(), window, cx)
                    })),
                );
            }
        }
        body
    }

    // -- §3.6 settings -----------------------------------------------------

    /// One labelled segmented control in a grouped card row.
    fn setting_row(
        &self,
        caption: String,
        control: impl IntoElement,
        cx: &App,
    ) -> gpui::AnyElement {
        v_flex()
            .w_full()
            .p(px(14.))
            .gap(px(8.))
            .child(text(caption, 13.).text_color(cx.theme().muted_foreground))
            .child(control)
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> Div {
        let mut appearance = material::segmented_track("appearance-track", cx);
        for value in ["system", "light", "dark"] {
            let selected = self.preferences.appearance.as_deref().unwrap_or("system") == value;
            appearance = appearance.child(
                material::segment(
                    SharedString::from(format!("appearance-{value}")),
                    label(value),
                    selected,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.preferences.appearance = (value != "system").then(|| value.to_owned());
                    this.host.save_preferences(&this.preferences);
                    this.apply_appearance(window, cx);
                    cx.notify();
                })),
            );
        }
        let mut languages = material::segmented_track("language-track", cx);
        for (value, title) in [
            ("system", label("system")),
            ("en", "English".into()),
            ("zh-CN", "简体中文".into()),
        ] {
            let selected = self.preferences.language.as_deref().unwrap_or("system") == value;
            languages = languages.child(
                material::segment(
                    SharedString::from(format!("language-{value}")),
                    title,
                    selected,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.preferences.language = (value != "system").then(|| value.to_owned());
                    this.host.save_preferences(&this.preferences);
                    tcode_ui::apply_locale(this.preferences.language.as_deref());
                    cx.notify();
                })),
            );
        }

        v_flex()
            .gap(px(20.))
            .child(material::grouped(
                vec![
                    self.setting_row(label("appearance"), appearance, cx),
                    self.setting_row(label("language"), languages, cx),
                    self.setting_row(
                        label("device_name"),
                        Input::new(&self.device_name)
                            .h(px(48.))
                            .min_h(px(48.))
                            .max_h(px(48.))
                            .rounded(px(12.))
                            .text_size(px(16.)),
                        cx,
                    ),
                ],
                cx,
            ))
            // About names the product with the wordmark, never "phone client".
            .child(material::grouped(
                vec![
                    v_flex()
                        .w_full()
                        .p(px(14.))
                        .gap(px(8.))
                        .child(text(label("about"), 13.).text_color(cx.theme().muted_foreground))
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .items_center()
                                .text_size(px(13.))
                                .line_height(px(18.))
                                .text_color(cx.theme().muted_foreground)
                                .child(material::brand_wordmark(cx))
                                .child(
                                    tr!("mobile.version", version = env!("CARGO_PKG_VERSION"))
                                        .into_owned(),
                                )
                                .child(
                                    tr!(
                                        "mobile.protocol",
                                        version = tcode_protocol::PROTOCOL_VERSION
                                    )
                                    .into_owned(),
                                ),
                        )
                        .into_any_element(),
                ],
                cx,
            ))
            .child(
                button("disconnect", label("disconnect"), false, true, cx)
                    .w_full()
                    .h(px(44.))
                    .text_size(px(15.))
                    .text_color(cx.theme().danger_foreground)
                    .on_click(cx.listener(|this, _, _, cx| this.disconnect(false, cx))),
            )
    }
}
