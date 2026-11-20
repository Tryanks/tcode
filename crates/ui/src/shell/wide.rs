use super::compact::back_button;
use super::*;

// ---------------------------------------------------------------------------
// Wide layout
// ---------------------------------------------------------------------------

impl AppShell {
    /// The hosts surface itself, without chrome: the same rows and the same
    /// pairing form at both widths. Compact pushes Pair onto the navigation
    /// stack; wide shows it in place, inside the Hosts route.
    /// The machines and the pairing form, each in its own layer, so the one
    /// left fades out under the one opened in its place.
    pub(super) fn hosts_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pairing = self.destination(cx) == Destination::Pair;
        let fade = crate::motion::CrossFade::new(
            "hosts-body",
            usize::from(pairing),
            2,
            CHANGE,
            window,
            cx,
        );
        let layers: Vec<(usize, AnyElement)> = (0..2)
            .filter(|layer| fade.shows(*layer))
            .map(|layer| {
                let surface = self.hosts_surface(layer == 1, window, cx);
                (
                    layer,
                    v_flex().size_full().child(surface).into_any_element(),
                )
            })
            .collect();
        fade.fill()
            .render(layers)
            .flex_1()
            .min_h_0()
            .w_full()
            .into_any_element()
    }

    /// The hosts rows, or the pairing form when `pairing`; nothing in a
    /// departed workspace while the arriving one draws them.
    pub(super) fn hosts_surface(
        &mut self,
        pairing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self
            .departing
            .as_ref()
            .is_some_and(|departing| departing.hosts_withheld)
        {
            return div().flex_1().into_any_element();
        }
        let hosts = self.hosts.clone();
        hosts.update(cx, |panel, cx| {
            if pairing {
                panel.render_pair(window, cx)
            } else {
                panel.render_hosts(window, cx)
            }
        })
    }

    /// The wide Hosts route's top strip: the page title, its close, and (on
    /// Windows) the caption cluster this route's content column owns, which
    /// sends the close to a row under the strip. The sidebar switches wide
    /// routes, so Machines has no Back control.
    pub(super) fn render_hosts_header(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::Hosts,
            self.window_state.read(cx).route(),
            false,
            RightTab::Diff,
        );
        let pairing = self.destination(cx) == Destination::Pair;
        let title: SharedString = if pairing {
            crate::tr!("hosts.pair.page_title").into_owned().into()
        } else {
            crate::tr!("hosts.title").into_owned().into()
        };
        // The sidebar switches to Machines and back; Pair is pushed over it,
        // so only Pair has somewhere to go back to.
        let gap = cx.theme().tokens.spacing.xs;
        let back =
            crate::motion::shown("hosts-back-shown", pairing.then_some(()), SHOW, window, cx).map(
                |shown| {
                    shown.fold_across_gap(
                        gap,
                        back_button("hosts-back", Destination::Hosts.back_label())
                            .debug_selector(|| "hosts-back".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.window_state
                                    .update(cx, |state, cx| state.close_pair(cx));
                            })),
                    )
                },
            );
        let title = div().flex_1().min_w_0().child(crate::motion::cross_fade(
            "hosts-title",
            title,
            CHANGE,
            window,
            cx,
            |title, _, cx| crate::material::bar_title(title.clone(), cx).into_any_element(),
        ));
        // Closes the route as the sidebar's Machines toggle does; without a
        // workspace beneath it there is nothing to close to.
        let close = self
            .window_state
            .read(cx)
            .history()
            .iter()
            .any(|destination| destination.route() != Route::Hosts)
            .then(|| {
                crate::widgets::button::close_button("hosts-close", crate::tr!("hosts.close"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.window_state.update(cx, |state, cx| {
                            state.back(cx);
                        });
                    }))
            });
        let (close, below_caption) = if hosts_caption {
            (
                None,
                close.map(|close| window_caption::below_caption(close, cx)),
            )
        } else {
            (close, None)
        };
        // The column is centred in the strip; whatever holds the strip's
        // right end is kept clear of it.
        let right_end = if hosts_caption {
            window_caption::CAPTION_CLUSTER_WIDTH
        } else {
            0.
        } + if close.is_some() {
            crate::chat::PANE_CLOSE_WIDTH
        } else {
            0.
        };
        let strip = window_drag_area(
            "hosts-header-drag",
            crate::settings_rows::content_header(
                crate::material::bar("hosts-header", crate::material::Bar::Strip, false, cx),
                cx,
            )
            .relative(),
            window,
            cx,
        )
        .child(
            crate::settings_rows::content_header_column()
                .items_center()
                .gap(gap)
                .pr(px(right_end))
                .children(back)
                .child(window_caption::drag_region(title)),
        )
        .child(
            h_flex()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .items_center()
                .when(!hosts_caption, |end| end.pr(cx.theme().tokens.spacing.sm))
                .children(close)
                .children(hosts_caption.then(|| window_caption::caption_controls(window, cx))),
        );
        v_flex()
            .flex_none()
            .w_full()
            .child(strip)
            .children(below_caption)
            .into_any_element()
    }

    /// The wide window: the workspace it is attached to, rising over the
    /// ones it left until it covers them. gpui-base has no transition for a
    /// whole tree being replaced; the window keeps the trees it left
    /// ([`Self::retire`]) and [`crate::motion::Scene`] draws them beneath.
    pub(super) fn render_wide(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let route = self.window_state.read(cx).route();
        let frame = self
            .attachments
            .sample("attachments", self.scene_key, window, cx);
        let keys: Vec<u64> = frame.scenes().copied().collect();
        self.departed
            .retain(|departed| keys.contains(&departed.key));
        let hosts_withheld = self.column.is_none() || route == Route::Hosts;
        let mut layers = Vec::with_capacity(keys.len());
        for key in keys {
            if key == self.scene_key {
                layers.push(self.render_workspace(route, window, cx));
                continue;
            }
            let Some(ix) = self.departed.iter().position(|left| left.key == key) else {
                layers.push(gpui::Empty.into_any_element());
                continue;
            };
            let mut left = self.departed.remove(ix);
            // Its keys would reach a thread the window has left.
            if left.column.as_ref().is_some_and(|column| {
                column
                    .all_panes(cx)
                    .iter()
                    .any(|pane| pane.focus_handle(cx).contains_focused(window, cx))
            }) {
                window.blur(cx);
            }
            std::mem::swap(&mut self.column, &mut left.column);
            std::mem::swap(&mut self.links, &mut left.links);
            std::mem::swap(&mut self.sidebar, &mut left.sidebar);
            self.departing = Some(Departing {
                destination: left.destination,
                hosts_withheld,
            });
            layers.push(self.render_workspace(left.route, window, cx));
            self.departing = None;
            std::mem::swap(&mut self.column, &mut left.column);
            std::mem::swap(&mut self.links, &mut left.links);
            std::mem::swap(&mut self.sidebar, &mut left.sidebar);
            self.departed.insert(ix, left);
        }
        frame.render(layers).into_any_element()
    }

    /// The wide window showing `route` of whatever `self.column` is.
    pub(super) fn render_workspace(
        &mut self,
        route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fullscreen = window.is_fullscreen();
        if self.column.is_none() {
            // No host: the threads' empty root, or Machines and Pair over it,
            // is the whole window. A tablet in landscape lands here before it
            // has a host and a desktop after its last one goes; neither has a
            // sidebar to switch routes with, so each page wears the same nav
            // bar as its compact self. Whatever the history says, nothing
            // else is shown until the window has a host.
            let page = match self.destination(cx) {
                Destination::Pair => self.render_pair_page(window, cx),
                Destination::Hosts => self.render_hosts_page(window, cx),
                _ => self.render_threads_page(window, cx),
            };
            return div()
                .id("app-shell")
                .size_full()
                // The theme's tint, not `material::canvas`, which clears it
                // over the macOS backdrop: this page keeps its tinted glass.
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .child(v_flex().id("hosts").size_full().child(page))
                .into_any_element();
        }
        let Some(column) = &mut self.column else {
            unreachable!("checked above");
        };
        let collapsed = DevicePreferences::sidebar_collapsed(cx);
        // The overlay is workspace-only transient state. Clear it synchronously
        // on route/expanded transitions rather than waiting for pointer input.
        if !collapsed || route != Route::Chat {
            column.sidebar_overlay_visible = false;
        }

        // A native WebView is not composited into GPUI and survives removal of
        // its layout node. Synchronize it before a route change or a
        // right-panel tab/close transition can unmount PreviewPanel.
        if self.departing.is_none() {
            for pane in column.all_panes(cx) {
                ThreadPanel::sync_preview(&pane, cx);
            }
        }

        let scene = column.routes.sample("routes", route, window, cx);
        let scenes: Vec<Route> = scene.scenes().copied().collect();
        // The workspace comes back as it is, not gliding from how it was.
        if !scenes.contains(&Route::Chat) {
            column.panel_glide.reset();
        }
        // The sidebar both workspace routes keep is drawn once, by the lowest
        // of them: that one is opaque, so the sidebar holds still while the
        // routes change around it.
        let sidebar_scene = scenes
            .iter()
            .copied()
            .find(|shown| !collapsed && *shown != Route::Settings);
        let layers: Vec<AnyElement> = scenes
            .into_iter()
            .map(|shown| {
                let sidebar = collapsed || sidebar_scene == Some(shown);
                // Settings draws its own rail where the workspace routes draw
                // their sidebar.
                let column = scene.unshared(&shown, |above| {
                    (*above == Route::Settings) == (shown == Route::Settings)
                });
                let sidebar = sidebar.then_some(column);
                match shown {
                    Route::Settings => self.render_settings_route(fullscreen, column, cx),
                    Route::Hosts => self.render_hosts_route(sidebar, fullscreen, window, cx),
                    Route::Chat => self.render_chat_route(sidebar, window, cx),
                }
            })
            .collect();
        scene.render(layers).into_any_element()
    }

    /// The Settings route, its rail at `rail` opacity.
    pub(super) fn render_settings_route(
        &mut self,
        fullscreen: bool,
        rail: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let link = self
            .settings_link(cx)
            .expect("a workspace route has a column");
        SettingsPage::fade_rail(&link.settings_page, rail, cx);
        // Settings is a window of its own: opening it switches the whole
        // window, so the settings rail — not the workspace sidebar — is the
        // left column. Root owns the translucent canvas across both routes;
        // Settings paints its own rail and content surfaces over it.
        div()
            .id("app-shell")
            .size_full()
            // Fullscreen flattens the canvas under the paper (see the
            // workspace root below).
            .when(fullscreen, |this| {
                this.bg(crate::material::opaque_canvas(cx))
            })
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            .on_action(cx.listener(Self::on_start_draft_for_project))
            .on_action(cx.listener(Self::on_quick_chat))
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .debug_selector(|| "settings-route".into())
                    .child(link.settings_page.clone()),
            )
            .into_any_element()
    }

    /// The Hosts route; `sidebar`, the opacity it draws the sidebar at, or
    /// none to leave its place empty for the route beneath it to show.
    pub(super) fn render_hosts_route(
        &mut self,
        sidebar: Option<f32>,
        fullscreen: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = DevicePreferences::sidebar_collapsed(cx);
        let column = self
            .column
            .as_ref()
            .expect("a workspace route has a column");
        let sidebar_width = column.sidebar_width.get();
        let sidebar = match sidebar {
            Some(opacity) => div()
                .size_full()
                .opacity(opacity)
                .child(self.layout_part("sidebar", self.sidebar.clone()))
                .into_any_element(),
            None => div().into_any_element(),
        };
        // The sidebar's column narrows away as it collapses and widens back,
        // the sidebar keeping to its far edge, as it does beside the chat.
        let shown_width = gpui_base::motion::transition(
            "hosts-route-sidebar",
            if collapsed { px(0.) } else { sidebar_width },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        // Hosts is a product surface, not a settings page: the sidebar stays,
        // because the feature area that leads here is meant to be persistent.
        let body = self.hosts_body(window, cx);
        let header = self.render_hosts_header(window, cx);
        let column = crate::window_seam::inset_from_seam(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .debug_selector(|| "hosts-route".into())
                .bg(crate::material::content_surface(cx))
                .shadow_sm(),
            "hosts-column-seam",
            gpui::Edges {
                left: collapsed,
                ..gpui::Edges::all(true)
            },
            window,
            cx,
        )
        .child(header)
        .child(body);
        div()
            .id("app-shell")
            .size_full()
            .when(fullscreen, |this| {
                this.bg(crate::material::opaque_canvas(cx))
            })
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            .on_action(cx.listener(Self::on_start_draft_for_project))
            .on_action(cx.listener(Self::on_quick_chat))
            .child(
                h_flex()
                    .id("workspace")
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .when(shown_width > px(0.), |row| {
                        row.child(
                            h_flex()
                                .flex_none()
                                .w(shown_width)
                                .h_full()
                                .justify_end()
                                .overflow_hidden()
                                .child(div().flex_none().w(sidebar_width).h_full().child(sidebar)),
                        )
                    })
                    .child(column),
            )
            .into_any_element()
    }

    /// The Chat route; `sidebar` as for [`Self::render_hosts_route`].
    pub(super) fn render_chat_route(
        &mut self,
        draw_sidebar: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette_open = self.window_state.read(cx).palette_mounted() && self.departing.is_none();
        let fullscreen = window.is_fullscreen();
        let collapsed = DevicePreferences::sidebar_collapsed(cx);
        let Some(column) = &mut self.column else {
            unreachable!("a workspace route has a column");
        };
        let split = column.split.clone();
        // The chat and the sidebar, which glides as it collapses and expands.
        let glide = column.panel_glide.sample(
            &[
                crate::motion::resize_glide::Panel::sized(column.sidebar_width.get(), !collapsed)
                    .kept(),
                crate::motion::resize_glide::Panel::flex(true),
            ],
            true,
            &split,
            window,
            cx,
        );

        // The chat's reading surface sits above the translucent canvas.
        let chat_panel = resizable_panel()
            .size_range(glide.range(1, gpui_base::PANEL_MIN_SIZE..Pixels::MAX))
            .child(glide.hold(
                1,
                self.layout_part("chat", self.render_chat_column(collapsed, window, cx)),
            ));
        let column = self.column.as_ref().expect("checked above");
        let remembered_sidebar = column.sidebar_width.clone();
        let sidebar = match draw_sidebar {
            Some(opacity) => div()
                .id("sidebar-focus-scope")
                .track_focus(&column.sidebar_focus)
                .size_full()
                .opacity(opacity)
                .child(self.layout_part("sidebar", self.sidebar.clone()))
                .into_any_element(),
            None => div().into_any_element(),
        };
        let overlay_width = column.sidebar_width.get();
        let overlay_visible = column.sidebar_overlay_visible;
        let palette = self.palette.clone();
        let group = h_resizable("workspace-panels")
            .with_state(&split)
            .with_handle_appearance(crate::widgets::handle_reset::appearance({
                let (split, remembered_sidebar) = (split.clone(), remembered_sidebar.clone());
                move |_, window, cx| {
                    if collapsed {
                        return;
                    }
                    let width = px(SIDEBAR_WIDTH);
                    split.update(cx, |state, cx| state.resize_panel(0, width, window, cx));
                    remembered_sidebar.set(width);
                }
            }))
            .on_resize(move |state, _, cx| {
                if !collapsed && let Some(size) = state.read(cx).sizes().first() {
                    remembered_sidebar.set(*size);
                }
            });
        // The sidebar is drawn once a frame: in its panel while that shows
        // it, gliding out included, and in the overlay only after.
        let sidebar_docked = glide.shows(0);
        let overlay_visible = overlay_visible && !sidebar_docked;
        let (docked_sidebar, overlay_sidebar) = match sidebar_docked {
            true => (Some(sidebar), None),
            false => (None, Some(sidebar)),
        };
        // The hover overlay slides in from the window's edge and back out,
        // drawn as it was while it goes.
        let overlay_in = gpui_base::motion::transition(
            "sidebar-hover-overlay-in",
            if overlay_visible { 1. } else { 0. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let overlay_leaving = !overlay_visible && overlay_in > 0. && collapsed;
        let workspace = div()
            .relative()
            .size_full()
            .child(
                group
                    .child(
                        resizable_panel()
                            .fixed()
                            .size(overlay_width)
                            .size_range(glide.range(0, px(SIDEBAR_MIN_WIDTH)..px(380.)))
                            .when_some(docked_sidebar, |panel, sidebar| {
                                panel.child(glide.hold(0, sidebar))
                            }),
                    )
                    .child(chat_panel),
            )
            .child(self.sidebar_overlay_presence(overlay_width, collapsed, cx))
            .when_some(
                overlay_sidebar.filter(|_| overlay_visible || overlay_leaving),
                |this, sidebar| {
                    this.child(
                        crate::material::floating_panel(
                            div().id("sidebar-hover-overlay"),
                            crate::material::Side::Left,
                            cx,
                        )
                        .debug_selector(|| "sidebar-hover-overlay".into())
                        .absolute()
                        .left(px(0.) - overlay_width * (1. - overlay_in))
                        .top_0()
                        .h_full()
                        .w(overlay_width)
                        .occlude()
                        .child(sidebar)
                        .when(overlay_leaving, |overlay| {
                            overlay.child(crate::motion::shield())
                        }),
                    )
                },
            )
            .into_any_element();

        // No separate titlebar: the sidebar and chat columns run to the window
        // top (the native traffic lights overlay the sidebar's top-left).
        div()
            .id("app-shell")
            .size_full()
            // Fullscreen only: a fullscreen Space has nothing but black behind
            // the vibrancy material, which muddies the translucent canvas —
            // cover it with its opaque base. Windowed, paint nothing here:
            // Root owns the translucent canvas.
            .when(fullscreen, |this| {
                this.bg(crate::material::opaque_canvas(cx))
            })
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            .on_action(cx.listener(Self::on_start_draft_for_project))
            .on_action(cx.listener(Self::on_quick_chat))
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .debug_selector(|| "chat-route".into())
                    .child(workspace),
            )
            .when(palette_open, |this| this.child(palette))
            .into_any_element()
    }

    // Ordinary hover is suppressed during scrolling, keyboard input and native
    // drag/drop. It cannot own the lifetime of the panel containing those interactions.
    pub(super) fn sidebar_overlay_presence(
        &self,
        width: Pixels,
        collapsed: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let shell = cx.entity().downgrade();
        canvas(
            move |bounds, window, _| {
                let edge = gpui::Bounds::new(
                    bounds.origin,
                    gpui::size(px(SIDEBAR_HOVER_EDGE), bounds.size.height),
                );
                window.insert_hitbox(
                    edge,
                    if collapsed {
                        HitboxBehavior::BlockMouse
                    } else {
                        HitboxBehavior::Normal
                    },
                )
            },
            move |bounds, edge, window, cx| {
                let panel = gpui::Bounds::new(bounds.origin, gpui::size(width, bounds.size.height));
                let update = Rc::new(
                    move |presence: Option<bool>, window: &mut Window, cx: &mut App| {
                        let _ = shell.update(cx, |shell, cx| {
                            if let Some(presence) = presence
                                && let Some(column) = &mut shell.column
                            {
                                column.sidebar_pointer_in_window = presence;
                            }
                            shell.update_sidebar_overlay(panel, &edge, window, cx);
                        });
                    },
                );
                window.defer(cx, {
                    let update = update.clone();
                    move |window, cx| update(None, window, cx)
                });
                window.on_mouse_event({
                    let update = update.clone();
                    move |_: &gpui::MouseMoveEvent, phase, window, cx| {
                        if phase.capture() {
                            update(Some(true), window, cx);
                        }
                    }
                });
                window.on_mouse_event({
                    let update = update.clone();
                    move |event: &gpui::FileDropEvent, phase, window, cx| {
                        if phase.capture() && matches!(event, gpui::FileDropEvent::Exited) {
                            update(Some(false), window, cx);
                        }
                    }
                });
                window.on_mouse_event(move |_: &gpui::MouseExitEvent, phase, window, cx| {
                    if phase.capture() {
                        update(Some(false), window, cx);
                    }
                });
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    pub(super) fn update_sidebar_overlay(
        &mut self,
        bounds: gpui::Bounds<Pixels>,
        edge: &gpui::Hitbox,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let held = self.sidebar.read(cx).holds_overlay(cx);
        let Some(column) = &mut self.column else {
            return;
        };
        let pointer = column.sidebar_pointer_in_window && !window.last_input_was_touch();
        let visible = DevicePreferences::sidebar_collapsed(cx)
            && self.window_state.read(cx).route() == Route::Chat
            && ((pointer && edge.is_hovered_at(window.mouse_position(), window))
                || (column.sidebar_overlay_visible
                    && ((pointer && bounds.contains(&window.mouse_position()))
                        || held
                        || column.sidebar_focus.contains_focused(window, cx))));
        if column.sidebar_overlay_visible != visible {
            column.sidebar_overlay_visible = visible;
            cx.notify();
        }
    }
}
