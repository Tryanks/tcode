use super::*;

impl MobileRoot {
    pub(super) fn render_hosts(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut content = v_flex().gap(px(12.)).px(px(16.)).py(px(12.));
        let last = self.host.last_host_id();
        for (i, host) in self.hosts.iter().enumerate() {
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
                .map(|t| {
                    tr!(
                        "mobile.connected_ago",
                        time = tcode_ui::time::humanize_ago(now_secs().saturating_sub(t))
                    )
                    .into_owned()
                })
                .unwrap_or_else(|| label("never_connected"));
            content = content.child(
                v_flex()
                    .id(("host", i))
                    .role(Role::Button)
                    .aria_label(host.name.clone())
                    .relative()
                    .child(long_press)
                    .min_h(px(72.))
                    .p(px(14.))
                    .gap(px(6.))
                    .rounded(px(12.))
                    .bg(cx.theme().secondary)
                    .active(|s| s.bg(cx.theme().foreground.opacity(0.08)))
                    .cursor_pointer()
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                text(host.name.clone(), 17.)
                                    .font_semibold()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate(),
                            )
                            .when(last.as_deref() == Some(&host.host_id), |d| {
                                d.child(
                                    text(label("last_host"), 12.)
                                        .px(px(8.))
                                        .rounded_full()
                                        .bg(cx.theme().secondary)
                                        .text_color(cx.theme().muted_foreground),
                                )
                            })
                            .child(text("›", 20.).text_color(cx.theme().muted_foreground)),
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
                    .child(
                        text(
                            tr!(
                                "mobile.fingerprint",
                                fingerprint =
                                    tcode_client::pairing::display_fingerprint(&host.fingerprint)
                            )
                            .into_owned(),
                            12.,
                        )
                        .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.connect(connect.clone(), cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            this.sheet = Some(Sheet::Remove(remove.clone()));
                            cx.notify();
                        }),
                    ),
            );
        }
        v_flex()
            .size_full()
            .child(
                text(label("hosts"), 28.)
                    .line_height(px(34.))
                    .font_semibold()
                    .px(px(16.))
                    .pt(px(24.))
                    .pb(px(16.)),
            )
            .child(if self.hosts.is_empty() {
                v_flex()
                    .flex_1()
                    .justify_center()
                    .px(px(24.))
                    .gap(px(14.))
                    .child(
                        text(label("hosts_empty"), 20.)
                            .font_semibold()
                            .text_center(),
                    )
                    .child(
                        text(label("hosts_help"), 16.)
                            .line_height(px(22.))
                            .text_center()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .into_any_element()
            } else {
                scroll("host-list", content).into_any_element()
            })
            .child(
                div().flex_none().p(px(16.)).child(
                    button("pair-new", label("pair_new"), true, true, cx)
                        .w_full()
                        .h(px(50.))
                        .on_click(cx.listener(|this, _, window, cx| this.open_pair(window, cx))),
                ),
            )
            .into_any_element()
    }
    pub(super) fn connection_badge(&self, cx: &App) -> Div {
        let changed = self
            .connected_host
            .as_ref()
            .is_some_and(|h| self.host.certificate_changed(&h.host_id));
        let (caption, color, spinning) = if changed {
            (
                label("certificate_changed"),
                cx.theme().danger_foreground,
                false,
            )
        } else if self.offline() {
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
        };
        h_flex()
            .gap(px(4.))
            .items_center()
            .text_color(color)
            .child(if spinning {
                spinner(14., color).into_any_element()
            } else {
                div()
                    .size(px(6.))
                    .rounded_full()
                    .bg(if self.online() {
                        cx.theme().success
                    } else {
                        color
                    })
                    .into_any_element()
            })
            .child(text(caption, 13.).when(!changed, |text| text.truncate()))
    }
    pub(super) fn render_threads(&self, cx: &mut Context<Self>) -> AnyElement {
        let heading = h_flex()
            .h(px(52.))
            .flex_none()
            .px(px(6.))
            .gap(px(2.))
            .child(
                button(
                    "back-hosts",
                    format!("‹ {}", label("hosts")),
                    false,
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.back(cx);
                })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .child(
                        text(
                            self.connected_host
                                .as_ref()
                                .map(|h| h.name.clone())
                                .unwrap_or_default(),
                            17.,
                        )
                        .font_semibold()
                        .truncate(),
                    )
                    .child(self.connection_badge(cx)),
            )
            .child(
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
                        .map(|s| s.read(cx).projects())
                        .unwrap_or_default();
                    if projects.len() == 1 {
                        this.start_draft(projects[0].clone(), window, cx);
                    } else {
                        this.sheet = Some(Sheet::Projects);
                        cx.notify();
                    }
                })),
            )
            .child(
                icon_button("settings", label("settings"), IconName::Settings, true, cx).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.sheet = Some(Sheet::Settings);
                        cx.notify();
                    }),
                ),
            );
        v_flex()
            .size_full()
            .child(heading)
            .child(div().flex_1().min_h_0().children(self.sidebar.clone()))
            .into_any_element()
    }
    pub(super) fn render_thread(&self, cx: &mut Context<Self>) -> AnyElement {
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
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(52.))
                    .flex_none()
                    .px(px(6.))
                    .gap(px(4.))
                    .child(
                        button(
                            "back-threads",
                            format!("‹ {}", label("threads")),
                            false,
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.back(cx);
                        })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(text(title, 17.).font_semibold().truncate())
                            .child(
                                text(project, 13.)
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate(),
                            ),
                    )
                    .when(!self.online(), |el| el.child(self.connection_badge(cx))),
            )
            .child(div().flex_1().min_h_0().children(self.chat.clone()))
            .into_any_element()
    }
    pub(super) fn render_sheet(
        &mut self,
        sheet: Sheet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = match &sheet {
            Sheet::Pair => label("pair_title"),
            Sheet::Remove(h) => tr!("mobile.remove_host", name = h.name).into_owned(),
            Sheet::Projects => label("choose_project"),
            Sheet::Settings => label("settings"),
        };
        let content = match sheet {
            Sheet::Pair => self.render_pair(cx),
            Sheet::Remove(host) => v_flex()
                .gap(px(16.))
                .child(text(label("remove_help"), 16.).text_color(cx.theme().muted_foreground))
                .child(
                    h_flex()
                        .gap(px(12.))
                        .child(
                            button("remove-cancel", label("cancel"), false, true, cx)
                                .flex_1()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.sheet = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            button("remove-confirm", label("remove"), false, true, cx)
                                .text_color(cx.theme().danger_foreground)
                                .flex_1()
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
                ),
            Sheet::Projects => {
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
                            .h(px(50.))
                            .justify_start()
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.start_draft(project.clone(), window, cx)
                                },
                            )),
                        );
                    }
                }
                body
            }
            Sheet::Settings => self.render_settings(cx),
        };
        let max_height = (window.viewport_size().height
            - px(70.)
            - self.host.safe_area().top
            - self.host.safe_area().bottom)
            .max(px(180.));
        div()
            .absolute()
            .inset_0()
            .bg(cx.theme().foreground.opacity(0.30))
            .flex()
            .flex_col()
            .justify_end()
            .child(
                v_flex()
                    .id("mobile-sheet")
                    .occlude()
                    .max_h(max_height)
                    .w_full()
                    .rounded_t(px(16.))
                    .bg(cx.theme().popover)
                    .shadow_lg()
                    .child(
                        h_flex()
                            .flex_none()
                            .min_h(px(60.))
                            .px(px(16.))
                            .gap(px(8.))
                            .child(text(title, 17.).font_semibold().flex_1().min_w_0())
                            .child(
                                button("sheet-cancel", label("cancel"), false, true, cx).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.pair.generation += 1;
                                        this.sheet = None;
                                        cx.notify();
                                    }),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .id("sheet-scroll")
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(
                                content
                                    .flex_none()
                                    .p(px(16.))
                                    .pb(px(16.) + self.host.safe_area().bottom),
                            ),
                    ),
            )
            .into_any_element()
    }
    fn render_settings(&self, cx: &mut Context<Self>) -> Div {
        let mut body = v_flex().gap(px(24.));
        let mut appearance = h_flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(12.))
            .bg(cx.theme().secondary);
        for value in ["system", "light", "dark"] {
            let selected = self.preferences.appearance.as_deref().unwrap_or("system") == value;
            appearance = appearance.child(
                button(
                    SharedString::from(format!("appearance-{value}")),
                    label(value),
                    false,
                    true,
                    cx,
                )
                .flex_1()
                .px(px(4.))
                .text_size(px(14.))
                .when(selected, |d| d.bg(cx.theme().popover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.preferences.appearance = (value != "system").then(|| value.to_owned());
                    this.host.save_preferences(&this.preferences);
                    this.apply_appearance(window, cx);
                    cx.notify();
                })),
            );
        }
        let mut languages = h_flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(12.))
            .bg(cx.theme().secondary);
        for (value, title) in [
            ("system", label("system")),
            ("en", "English".into()),
            ("zh-CN", "简体中文".into()),
        ] {
            let selected = self.preferences.language.as_deref().unwrap_or("system") == value;
            languages = languages.child(
                button(
                    SharedString::from(format!("language-{value}")),
                    title,
                    false,
                    true,
                    cx,
                )
                .flex_1()
                .px(px(4.))
                .text_size(px(14.))
                .when(selected, |d| d.bg(cx.theme().popover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.preferences.language = (value != "system").then(|| value.to_owned());
                    this.host.save_preferences(&this.preferences);
                    tcode_ui::apply_locale(this.preferences.language.as_deref());
                    cx.notify();
                })),
            );
        }
        body = body
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(text(label("appearance"), 13.).text_color(cx.theme().muted_foreground))
                    .child(appearance),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(text(label("language"), 13.).text_color(cx.theme().muted_foreground))
                    .child(languages),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(text(label("device_name"), 13.).text_color(cx.theme().muted_foreground))
                    .child(
                        Input::new(&self.device_name)
                            .h(px(48.))
                            .min_h(px(48.))
                            .max_h(px(48.))
                            .rounded(px(12.))
                            .text_size(px(16.)),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(text(label("about"), 13.).text_color(cx.theme().muted_foreground))
                    .child(text(label("phone_name"), 17.).font_semibold())
                    .child(text(
                        tr!("mobile.version", version = env!("CARGO_PKG_VERSION")).into_owned(),
                        14.,
                    ))
                    .child(
                        text(
                            tr!(
                                "mobile.protocol",
                                version = tcode_protocol::PROTOCOL_VERSION
                            )
                            .into_owned(),
                            14.,
                        )
                        .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(
                button("disconnect", label("disconnect"), false, true, cx)
                    .text_size(px(14.))
                    .text_color(cx.theme().danger_foreground)
                    .on_click(cx.listener(|this, _, _, cx| this.disconnect(false, cx))),
            );
        body
    }
}
