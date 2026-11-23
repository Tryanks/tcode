use super::*;

impl SessionsSidebar {
    pub(super) fn render_app_row(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let traffic_lights = crate::window_caption::traffic_light_inset(window);
        window_drag_area(
            "sidebar-app-row-drag",
            bar("sidebar-app-row", Bar::Strip, false, cx)
                .when(traffic_lights > px(0.), |strip| strip.pl(traffic_lights)),
            window,
            cx,
        )
        .child(crate::material::brand_wordmark(cx))
        // The collapse toggle lives in the chat header (`crate::chat`), not
        // here: collapsing takes the sidebar to zero width, so a control that
        // rode the sidebar would take itself off screen.
        .child(div().flex_1())
    }

    pub(super) fn render_search_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shortcut = crate::widgets::kbd::Kbd::new(gpui::Keystroke {
            modifiers: gpui::Modifiers::secondary_key(cx),
            key: "k".into(),
            key_char: None,
        });
        div().flex_none().px_2().pb_1().child(
            self.search_field(
                "sidebar-search",
                crate::tr!("sidebar.search").into(),
                false,
                cx,
            )
            .child(div().flex_none().child(shortcut)),
        )
    }

    /// The search control at the head of either sidebar. It opens the
    /// palette rather than taking text, so it is a button drawn as a quiet
    /// search field: an input's height and radius in the muted fill, with no
    /// border to set it apart from the rows around it, and the search glyph
    /// and label in the placeholder tone. A pointer brings the rows' hover
    /// fill in over it; a touch layout, which has no pointer, dims it while
    /// pressed.
    pub(super) fn search_field(
        &self,
        id: &'static str,
        label: SharedString,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> crate::material::Clickable<gpui::Div> {
        let theme = cx.theme();
        let (fill, hover, placeholder) = (theme.muted, theme.list_hover, theme.muted_foreground);
        let gap = theme.tokens.spacing.sm;
        let radius = crate::material::radius_input(cx);
        crate::material::accessible_clickable(h_flex(), id, Role::Button, label.clone(), cx)
            .debug_selector(move || id.into())
            .relative()
            .h(px(crate::material::input_height(compact)))
            .px(crate::material::selectable_row_inset(compact))
            .gap(gap)
            .items_center()
            .rounded(radius)
            .bg(fill)
            .text_body(compact, cx)
            .text_color(placeholder)
            .cursor_pointer()
            .when(compact, |field| field.active(|s| s.opacity(0.8)))
            .child(crate::motion::hover_layer(
                None,
                hover.opacity(0.),
                hover,
                radius,
                crate::material::HOVER.into(),
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_palette(cx));
            }))
            .child(Icon::new(IconName::Search).flex_none())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(label)),
            )
    }

    /// The control that leads to Machines: over one host, that host's
    /// machine and how its link is; over several, whose headings say that
    /// of each, Machines itself. Compact has none: the threads page's nav
    /// bar leads there.
    pub(super) fn render_machine_control(
        &self,
        lead: &Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.window_state.read(cx).route() == Route::Hosts;
        let store = lead.read(cx);
        let connection_state = store.connection_state();

        // Local, the control says "This machine" and the tooltip its name;
        // remote, the control names the machine and the tooltip the link.
        let (name, detail): (SharedString, Option<SharedString>) = if self.sectioned() {
            (crate::tr!("hosts.title").into_owned().into(), None)
        } else {
            (
                store.host_title(),
                match store.host_key() {
                    HostKey::Remote(_) => {
                        Some(crate::remote::connection_label(&connection_state).into())
                    }
                    HostKey::Local => crate::remote::local_machine_name(cx).map(SharedString::from),
                },
            )
        };
        let dot = (!self.sectioned()).then(|| {
            transition(
                ("sidebar-hosts", "connection"),
                cx.theme().connection_color(&connection_state),
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            )
        });
        let tooltip_name = name.clone();
        let control = Button::new("sidebar-hosts")
            .ghost()
            .selected(active)
            .w_full()
            .aria_label(crate::tr!("hosts.title"))
            .debug_selector(|| "sidebar-feature-hosts".into())
            // A toggle: on Machines (or Pair over it) it closes the route,
            // back to the workspace the window was showing.
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state.update(cx, |state, cx| {
                    if state.route() == Route::Hosts {
                        state.back(cx);
                    } else {
                        state.go(Destination::Hosts, cx);
                    }
                });
            }))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap(cx.theme().tokens.spacing.sm)
                    .items_center()
                    .text_body(false, cx)
                    .children(dot.map(crate::material::status_dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(name)),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .flex_none()
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            // `Button::tooltip` takes text; this one has two lines.
            .map(|control| {
                control.with_tooltip(move |_, _| {
                    let (name, detail) = (tooltip_name.clone(), detail.clone());
                    Tooltip::element(move |_, cx| {
                        v_flex()
                            .child(name.clone())
                            .children(detail.clone().map(|detail| {
                                div().text_color(cx.theme().muted_foreground).child(detail)
                            }))
                    })
                })
            });
        div().flex_none().px_2().pb_1().child(control)
    }

    pub(super) fn render_layout_toggle(
        &self,
        layout: SidebarLayout,
        cx: &mut Context<Self>,
    ) -> Button {
        let tooltip = match layout {
            SidebarLayout::Flat => crate::tr!("sidebar.layout_grouped"),
            SidebarLayout::Grouped => crate::tr!("sidebar.layout_flat"),
        };
        crate::material::toolbar_icon_button(
            "toggle-sidebar-layout",
            IconName::LayoutDashboard,
            tooltip.clone(),
            self.compact(cx),
        )
        .aria_label(tooltip)
        .on_click(move |_, _, cx| {
            let next = match layout {
                SidebarLayout::Flat => SidebarLayout::Grouped,
                SidebarLayout::Grouped => SidebarLayout::Flat,
            };
            DevicePreferences::set_sidebar_layout(next, cx);
        })
    }

    /// The "add project" control, which a caller points at a host.
    pub(super) fn add_project_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("add-project")
            .ghost()
            .xsmall()
            .compact()
            .icon(
                Icon::empty()
                    .path("icons/folder-plus.svg")
                    .text_color(cx.theme().muted_foreground),
            )
            .tooltip(crate::tr!("sidebar.add_project"))
    }

    /// The project view's header. Over one host its sort, quick chat and
    /// new project are that host's; over several, each host's heading
    /// carries its own.
    pub(super) fn render_projects_header(
        &self,
        lead: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let host = lead.read(cx).host_key();
        let sort_label = crate::settings::project_sort_label(lead.read(cx).project_sort());
        let sort = lead.clone();
        let one_host = !self.sectioned();
        bar("sidebar-projects-header", Bar::Section, false, cx)
            .items_center()
            .justify_end()
            .child(
                h_flex()
                    .gap(cx.theme().tokens.spacing.xxs)
                    .when(one_host, |row| {
                        row.child(
                            Button::new("sort-projects")
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::SortAscending)
                                .tooltip(crate::tr!("sidebar.sort", mode = sort_label))
                                .on_click(move |_, _, cx| {
                                    sort.update(cx, |store, cx| {
                                        store.cycle_project_sort(cx);
                                    });
                                }),
                        )
                    })
                    .child(self.render_layout_toggle(SidebarLayout::Grouped, cx))
                    .when(one_host && lead.read(cx).scope().is_full(), |row| {
                        let quick_chat = lead.clone();
                        row.child(
                            Button::new("quick-chat-grouped")
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::Plus)
                                .tooltip(crate::tr!("sidebar.quick_chat"))
                                .on_click(move |_, _, cx| {
                                    quick_chat.update(cx, |store, cx| store.start_scratch_draft(cx))
                                }),
                        )
                        .child(self.add_project_button(cx).on_click(cx.listener(
                            move |this, _, window, cx| this.add_project(&host, window, cx),
                        )))
                    }),
            )
    }

    /// Each host whose threads the list shows.
    pub(super) fn host_projects(&self, cx: &App) -> Rc<Vec<HostProjects>> {
        let sectioned = self.sectioned();
        Rc::new(
            self.hosts
                .iter()
                .filter_map(|host| {
                    let store = host.store.read(cx);
                    if sectioned && host_down(store) {
                        return None;
                    }
                    let projects = store
                        .projects()
                        .into_iter()
                        .map(|project| {
                            let key = ProjectKey {
                                host: host.key.clone(),
                                project_id: project.id.clone(),
                            };
                            (key, project)
                        })
                        .collect();
                    Some(HostProjects {
                        host: host.key.clone(),
                        title: store.host_title(),
                        full: store.scope().is_full(),
                        projects,
                    })
                })
                .collect(),
        )
    }

    /// `trigger` opens the choice of where a new thread starts: each host's
    /// quick thread and projects, under its name over several hosts.
    pub(crate) fn new_thread_chooser(
        &self,
        trigger: Button,
        touch: bool,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let hosts = self.host_projects(cx);
        let sectioned = self.sectioned();
        trigger
            .dropdown_menu(move |menu, _, _| {
                hosts.iter().fold(menu, |menu, host| {
                    let menu = host_section(menu, sectioned, &host.title).when(host.full, |menu| {
                        menu.menu(
                            crate::tr!("sidebar.quick_chat"),
                            Box::new(QuickChat(host.host.clone())),
                        )
                    });
                    host.projects.iter().fold(menu, |menu, (key, project)| {
                        menu.menu(
                            project.name.clone(),
                            Box::new(StartDraftForProject(key.clone())),
                        )
                    })
                })
            })
            .touch(touch)
    }

    /// The threads view's header. Its filter and new thread list every
    /// host's projects, under each host's name when there are several, as
    /// its new project lists the hosts that hold their projects.
    pub(super) fn render_flat_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sectioned = self.sectioned();
        let hosts = self.host_projects(cx);
        let active_filter = self.project_filter.clone();
        let filter_icon_color = gpui_base::motion::transition(
            "sidebar-filter-icon",
            if active_filter.is_some() {
                cx.theme().primary
            } else {
                cx.theme().muted_foreground
            },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let filter_hosts = hosts.clone();
        let filter_for_menu = active_filter.clone();
        let filter_button = Button::new("filter-sidebar-project")
            .ghost()
            .xsmall()
            .compact()
            .icon(Icon::new(IconName::Folder).text_color(filter_icon_color))
            .tooltip(crate::tr!("sidebar.filter_project"))
            .dropdown_menu(move |menu, _window, _cx| {
                let mut menu = menu.menu_with_check(
                    crate::tr!("sidebar.all_projects").into_owned(),
                    filter_for_menu.is_none(),
                    Box::new(FilterProject(None)),
                );
                for host in filter_hosts.iter() {
                    menu = host_section(menu, sectioned, &host.title);
                    for (key, project) in &host.projects {
                        menu = menu.menu_with_check(
                            project.name.clone(),
                            filter_for_menu.as_ref() == Some(key),
                            Box::new(FilterProject(Some(key.clone()))),
                        );
                    }
                }
                menu
            });

        let mut projects = hosts.iter().flat_map(|host| &host.projects);
        let draft_project = match &active_filter {
            Some(filter) => projects.find(|(key, _)| key == filter),
            // Over several hosts every other host's quick thread is a choice
            // too.
            None if !sectioned => projects.next().filter(|_| projects.next().is_none()),
            None => None,
        }
        .cloned();
        let new_thread = Button::new("new-flat-thread")
            .ghost()
            .xsmall()
            .compact()
            .icon(IconName::Plus);
        let new_thread = if let Some((key, project)) = draft_project {
            let tooltip = hosts
                .iter()
                .find(|host| host.host == key.host)
                .filter(|_| sectioned)
                .map_or(crate::tr!("sidebar.create_thread"), |host| {
                    crate::tr!("sidebar.create_thread_on", host = host.title.clone())
                });
            let cwd = project.root;
            new_thread
                .tooltip(tooltip)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(store) = this.store_of(&key.host) {
                        store.update(cx, |store, cx| {
                            store.start_draft(key.project_id.clone(), cwd.clone(), cx);
                        });
                    }
                }))
                .into_any_element()
        } else {
            self.new_thread_chooser(
                new_thread.tooltip(crate::tr!("sidebar.create_thread")),
                false,
                cx,
            )
            .into_any_element()
        };

        let full: Vec<(HostKey, SharedString)> = hosts
            .iter()
            .filter(|host| host.full)
            .map(|host| (host.host.clone(), host.title.clone()))
            .collect();
        let add_project = match full.as_slice() {
            [] => None,
            // Over several hosts the menu names the host before it is asked.
            [(host, _)] if !sectioned => {
                let host = host.clone();
                Some(
                    self.add_project_button(cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_project(&host, window, cx)
                        }))
                        .into_any_element(),
                )
            }
            _ => Some(
                self.add_project_button(cx)
                    .dropdown_menu(move |menu, _window, _cx| {
                        full.iter().fold(menu, |menu, (host, title)| {
                            menu.menu(title.clone(), Box::new(AddProject(host.clone())))
                        })
                    })
                    .into_any_element(),
            ),
        };

        bar("sidebar-flat-header", Bar::Section, false, cx)
            .items_center()
            .justify_end()
            .child(
                h_flex()
                    .gap(cx.theme().tokens.spacing.xxs)
                    .child(filter_button)
                    .child(self.render_layout_toggle(SidebarLayout::Flat, cx))
                    .child(new_thread)
                    .children(add_project),
            )
    }

    pub(super) fn render_group_header(
        &self,
        host: &HostKey,
        group: &ProjectGroup,
        flags: &HashMap<String, ThreadFlags>,
        collapsed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(store) = self.store_of(host) else {
            return div().into_any_element();
        };
        let project = ProjectKey {
            host: host.clone(),
            project_id: group.project.id.clone(),
        };
        let name_id = self.name(host, &project.project_id).into_owned();
        let has_unread = group.sessions.iter().any(|meta| {
            meta.parent_session_id.is_none()
                && flags.get(&meta.id).is_some_and(|flags| flags.unread)
        });
        let group_key = format!("group-{name_id}");

        let share = self.share_target(host, Some(&project.project_id), cx);
        let header_toggle = project.clone();
        let plus_cwd = group.project.root.clone();
        let plus_project_id = project.project_id.clone();
        let menu_project = project.clone();
        let can_archive = group
            .sessions
            .iter()
            .any(|meta| !flags.get(&meta.id).is_some_and(|flags| flags.working));

        let quick_chat = group.project.quick_chat;
        let name = crate::project_icon::group_label(&group.project);
        let header_label = if quick_chat {
            name.clone()
        } else {
            crate::tr!("sidebar.project", name = name.clone()).into_owned()
        };
        let new_thread_label: SharedString = if quick_chat {
            crate::tr!("sidebar.quick_chat")
        } else {
            crate::tr!("sidebar.create_thread")
        }
        .into();
        // The "+" fades in under the pointer, and as the keyboard comes onto
        // it.
        let plus_id = gpui::ElementId::from(SharedString::from(format!("new-thread-{name_id}")));
        let (plus_focus, plus_focused) =
            crate::motion::focus_ring::focus_reveal(&plus_id, true, window, cx);
        let hover = self
            .hover_progress(&group_key, window, cx)
            .max(plus_focused);
        // The dot shows while unread or fading out, and gives its slot to
        // the "+" under the pointer.
        let unread_dot = crate::motion::shown(
            SharedString::from(format!("project-unread-{name_id}")),
            has_unread.then_some(()),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fade(
                div()
                    .opacity(1. - hover)
                    .child(div().size(px(6.)).rounded_full().bg(cx.theme().primary)),
            )
        });
        let header = selectable_row(
            crate::material::accessible_clickable(
                h_flex(),
                gpui::SharedString::from(format!("project-header-{name_id}")),
                Role::Button,
                header_label,
                cx,
            ),
            false,
            false,
            cx,
        )
        .aria_expanded(!collapsed)
        .debug_selector({
            let name_id = name_id.clone();
            move || {
                if quick_chat {
                    "quick-chats-header".into()
                } else {
                    format!("project-header-{name_id}")
                }
            }
        })
        .text_body(false, cx)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_project(&header_toggle, cx);
        }))
        .child(collapse_chevron(
            self.fold_progress(&fold_key(Fold::Project, &name_id), !collapsed),
            cx,
        ))
        .child(crate::project_icon::artwork(
            store.read(cx).image_host(),
            &group.project,
            16.,
        ))
        .child(
            sidebar_label(name)
                .font_medium()
                .text_color(cx.theme().sidebar_foreground),
        )
        .children(self.shared_badge(&project, window, cx))
        // Unread dot when any child thread is unread.
        .children(unread_dot)
        .child({
            let store = store.clone();
            Button::new(plus_id)
                .track_focus(&plus_focus)
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
                .aria_label(new_thread_label.clone())
                .tooltip(new_thread_label)
                // Opacity keeps the button a tab stop, as the settle button does.
                .opacity(hover)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let cwd = plus_cwd.clone();
                    let project_id = plus_project_id.clone();
                    store.update(cx, |store, cx| {
                        if quick_chat {
                            store.start_scratch_draft(cx);
                        } else {
                            store.start_draft(project_id, cwd, cx);
                        }
                    });
                })
        });
        let header = self.track_hover(header, &group_key);
        if quick_chat {
            return header.into_any_element();
        }
        let scope = store.read(cx).scope().clone();
        // Another machine's project is not on this client to reveal.
        let local = !store.read(cx).is_remote();
        header
            .context_menu(move |menu, _window, cx| {
                let id = menu_project.clone();
                let delete_label = crate::tr!("sidebar.remove_project").into_owned();
                menu.when(scope.is_full(), |menu| {
                    menu.menu(
                        crate::tr!("project_icon.title"),
                        Box::new(ChangeProjectIcon(id.clone())),
                    )
                    .menu(
                        crate::tr!("sidebar.change_project_root"),
                        Box::new(ChangeProjectRoot(id.clone())),
                    )
                })
                .when(scope.is_full(), |menu| {
                    menu.menu(
                        crate::tr!("sidebar.project_thread_rules"),
                        Box::new(ProjectThreadRules(id.clone())),
                    )
                })
                .menu_with_enable(
                    crate::tr!("sidebar.archive_all").into_owned(),
                    Box::new(ProjectArchiveAll(id.clone())),
                    can_archive,
                )
                .when(scope.is_full(), |menu| {
                    menu.menu_element(Box::new(ProjectDelete(id.clone())), move |_window, cx| {
                        div()
                            .flex_1()
                            .text_color(cx.theme().danger)
                            .child(crate::widgets::fade_text(delete_label.clone()))
                    })
                })
                .when(local, |menu| {
                    menu.menu(
                        crate::tr!("sidebar.reveal_project").into_owned(),
                        Box::new(ProjectReveal(id)),
                    )
                })
                .when_some(share.as_ref(), |menu, share| {
                    spaces::share_items(menu, share, false, cx)
                })
            })
            .touch(false)
            .reveals(&self.hover, group_key)
            .into_any_element()
    }

    /// The parts of `host`'s heading every layout draws: its link's dot,
    /// how the link is while it is not up, Pair again while the host refuses
    /// this device, and the menu of its actions.
    pub(super) fn host_heading_parts(
        &self,
        host: &HostKey,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<HostHeadingParts> {
        let store = self.store_of(host)?;
        let store = store.read(cx);
        let state = store.connection_state();
        let failure = crate::remote::connection_failure(state.clone());
        let up = failure.is_none();
        let title = store.host_title();
        let menu = HostMenu {
            host: host.clone(),
            projects: (up && store.scope().is_full()).then(|| {
                store
                    .projects()
                    .into_iter()
                    .map(|project| {
                        let key = ProjectKey {
                            host: host.clone(),
                            project_id: project.id,
                        };
                        (key, project.name)
                    })
                    .collect()
            }),
            sort: up.then(|| crate::settings::project_sort_label(store.project_sort())),
        };
        let status = (!matches!(
            state,
            tcode_client::ConnectionState::Connected { .. }
                | tcode_client::ConnectionState::Syncing { .. }
        ))
        .then(|| crate::remote::connection_label_beside_pair_again(&state));
        let refused = failure == Some(tcode_client::ConnectionFailure::AuthenticationRejected);
        let id = host.name();
        let part = |part: &str| SharedString::from(format!("host-{part}-{id}"));
        let dot = transition(
            (part("dot"), "connection"),
            cx.theme().connection_color(&state),
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let status_id = part("status");
        let status =
            crate::motion::shown(status_id.clone(), status, SHOW, window, cx).map(|shown| {
                shown.fold(
                    div()
                        .debug_selector(move || status_id.to_string())
                        .text_secondary(compact, cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(shown.value.clone())),
                )
            });
        let pair = crate::motion::shown(part("pair"), refused.then_some(()), SHOW, window, cx).map(
            |shown| {
                let pair_id = part("pair-again");
                shown.fade(
                    Button::new(pair_id.clone())
                        .debug_selector(move || pair_id.to_string())
                        .primary()
                        .xsmall()
                        .compact()
                        .label(crate::tr!("hosts.pair_again"))
                        // A new pairing needs a new invitation from the machine.
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.window_state
                                .update(cx, |state, cx| state.go(Destination::Pair, cx));
                        })),
                )
            },
        );
        let menu_id = gpui::ElementId::from(part("menu"));
        let (menu_focus, menu_focused) =
            crate::motion::focus_ring::focus_reveal(&menu_id, true, window, cx);
        let reveal = if compact {
            1.
        } else {
            self.hover_progress(&format!("host-{id}"), window, cx)
                .max(menu_focused)
        };
        let menu = Button::new(menu_id)
            .track_focus(&menu_focus)
            .ghost()
            .xsmall()
            .icon(Icon::new(IconName::Ellipsis).text_color(cx.theme().muted_foreground))
            .aria_label(crate::tr!("hosts.actions", name = title.clone()))
            // Opacity keeps the button a tab stop, as a project's "+" does.
            .opacity(reveal)
            .dropdown_menu(move |popup, _, _| menu.build(popup))
            .touch(compact);
        Some(HostHeadingParts {
            title,
            dot,
            status,
            pair,
            // The heading must not arm its own press before the menu opens.
            menu: div()
                .flex_none()
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(menu)
                .into_any_element(),
        })
    }

    /// The heading of `host`'s section of a list over several hosts.
    /// `collapsed` is `None` where nothing under it folds.
    pub(super) fn render_host_heading(
        &self,
        host: &HostKey,
        collapsed: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(parts) = self.host_heading_parts(host, false, window, cx) else {
            return div().into_any_element();
        };
        let id = host.name().to_owned();
        let group = format!("host-{id}");
        let toggle = host.clone();
        let heading = selectable_row(
            crate::material::accessible_clickable(
                h_flex(),
                SharedString::from(format!("host-heading-{id}")),
                Role::Button,
                crate::tr!("sidebar.host", name = parts.title.clone()),
                cx,
            ),
            false,
            false,
            cx,
        )
        .debug_selector({
            let id = id.clone();
            move || format!("host-heading-{id}")
        })
        .text_body(false, cx)
        .when_some(collapsed, |heading, collapsed| {
            heading
                .aria_expanded(!collapsed)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_host(&toggle, cx)))
                .child(collapse_chevron(
                    self.fold_progress(&fold_key(Fold::Host, &id), !collapsed),
                    cx,
                ))
        })
        .child(crate::material::status_dot(parts.dot))
        .child(
            sidebar_label(parts.title)
                .font_semibold()
                .text_color(cx.theme().sidebar_foreground),
        )
        .children(parts.pair)
        .child(parts.menu);
        v_flex()
            .w_full()
            .child(self.track_hover(heading, &group))
            .children(
                parts
                    .status
                    .map(|status| div().pl(px(THREAD_INDENT)).pr_2().child(status)),
            )
            .into_any_element()
    }

    pub(super) fn render_grouped_row(
        &self,
        index: usize,
        row: &GroupedListRow,
        lists: &[HostLists],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let item = v_flex().w_full().px_2().when(
            index > 0 && matches!(row, GroupedListRow::Project { .. }),
            |item| item.pt(px(2.)),
        );
        let project = |host: &usize, group: &usize| {
            let of = &lists[*host];
            (of, &of.groups[*group].project)
        };
        match row {
            GroupedListRow::Host { host, collapsed } => item
                .when(index > 0, |item| item.pt(cx.theme().tokens.spacing.sm))
                .child(self.render_host_heading(&lists[*host].host, Some(*collapsed), window, cx))
                .into_any_element(),
            GroupedListRow::Project {
                host,
                group,
                collapsed,
            } => {
                let of = &lists[*host];
                item.child(self.render_group_header(
                    &of.host,
                    &of.groups[*group],
                    &of.flags,
                    *collapsed,
                    window,
                    cx,
                ))
                .into_any_element()
            }
            GroupedListRow::Thread(row) => {
                let Some(of) = lists.iter().find(|of| of.host == row.host) else {
                    return item.into_any_element();
                };
                item.child({
                    let thread = self.render_thread(row, &of.flags, window, cx);
                    drag_ghost::slot(arrange::thread_slot(&row.name), thread, window, cx)
                })
                .into_any_element()
            }
            GroupedListRow::ToggleThreads {
                host,
                group,
                expanded,
                label,
            } => {
                let (of, project) = project(host, group);
                let name = self.name(&of.host, &project.id).into_owned();
                let toggle = ProjectKey {
                    host: of.host.clone(),
                    project_id: project.id.clone(),
                };
                item.child(
                    div().w_full().pl(px(THREAD_INDENT)).child(
                        Button::new(SharedString::from(format!("show-more-{name}")))
                            .text()
                            .xsmall()
                            .label(label.clone())
                            .aria_expanded(*expanded)
                            .debug_selector(move || format!("show-more-{name}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded_groups.remove(&toggle) {
                                    this.expanded_groups.insert(toggle.clone());
                                }
                                cx.notify();
                            })),
                    ),
                )
                .into_any_element()
            }
            GroupedListRow::Settled { host, group, count } => {
                let (of, project) = project(host, group);
                let shelf = Shelf::Project(ProjectKey {
                    host: of.host.clone(),
                    project_id: project.id.clone(),
                });
                item.child(self.render_settled_header(&shelf, *count, None, window, cx))
                    .into_any_element()
            }
            GroupedListRow::SettledMore { host, group, count } => {
                let (of, project) = project(host, group);
                let shelf = Shelf::Project(ProjectKey {
                    host: of.host.clone(),
                    project_id: project.id.clone(),
                });
                item.child(self.render_settled_more(&shelf, *count, cx))
                    .into_any_element()
            }
            GroupedListRow::Hint(hint) => item
                .pb_2()
                .child(empty_row(hint.clone(), cx))
                .into_any_element(),
        }
    }

    pub(super) fn thread_row_state(
        &self,
        store: &WorkspaceStore,
        row: &ListedThread,
        flags: &HashMap<String, ThreadFlags>,
        row_key: String,
    ) -> ThreadRowState {
        let meta = &row.meta;
        let thread = row.key();
        let own_flags = flags.get(&meta.id).cloned().unwrap_or_default();
        ThreadRowState {
            renaming: self
                .renaming
                .as_ref()
                .filter(|rename| rename.thread == thread)
                .map(|rename| rename.input.clone()),
            thread,
            name: row.name.clone(),
            row_key,
            waiting_for_approval: own_flags.waiting_for_approval,
            waiting_for_input: own_flags.waiting_for_input,
            settle_blocked: own_flags.settle_blocked,
            background: own_flags.background,
            // A run that failed with a message scheduled after it is waiting for that
            // message: a turn that hits its usage limit ends failed and queues its own resume.
            failed: own_flags.failed && own_flags.wakes.scheduled_at.is_none(),
            wakes: own_flags.wakes,
            waiting_agent: own_flags.waiting_agent,
            native_asking: own_flags.native_asking,
            auto_settle_enabled: meta.auto_settle_disabled_at.is_none(),
            is_worktree: meta.worktree.is_some(),
            show_unread: own_flags.unread && !own_flags.working,
            menu_can_fork: own_flags.forks,
            title_generating: store.title_generating(&meta.id),
            draft: meta.draft_text.is_some(),
            draft_kept: meta
                .draft_text
                .as_ref()
                .is_some_and(|text| !text.is_empty()),
        }
    }
}
