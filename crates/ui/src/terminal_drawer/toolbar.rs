use super::*;

impl TerminalDrawer {
    pub(super) fn render_tab_strip(
        &self,
        tabs: &[(u64, String, bool, bool)],
        active_id: Option<u64>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> TabBar {
        let mut tab_strip = TabBar::new("terminal-tab-list")
            .underline()
            .hairline(false)
            .compact(compact)
            .label_max_w(px(160.))
            .aria_label(crate::tr!("terminal.tabs"))
            .flex_1()
            .min_w_0()
            .mb(px(-1.));
        for (id, label, _, bell) in tabs {
            let id = *id;
            let selected = active_id == Some(id);
            let close_id = id;
            tab_strip = tab_strip.item(
                TabItem::new(("terminal-tab", id), label.clone())
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.workspace_store.update(cx, |store, _cx| {
                            store.activate_terminal_for(
                                this.session_id.clone().unwrap_or_default(),
                                id,
                            )
                        });
                    }))
                    .trailing({
                        let drawer = cx.entity().downgrade();
                        let bell = *bell;
                        move |window, cx| {
                            let drawer = drawer.clone();
                            h_flex()
                                // A bell's dot folds in before the close button,
                                // and away once the terminal is answered.
                                .children(
                                    crate::motion::shown(
                                        ("terminal-bell", id),
                                        bell.then_some(()),
                                        SHOW,
                                        window,
                                        cx,
                                    )
                                    .map(|shown| {
                                        shown.fold_across(
                                            div()
                                                .pr(cx.theme().tokens.spacing.xxs)
                                                .text_token(&cx.theme().tokens.typography.xs)
                                                .text_color(cx.theme().warning)
                                                .child("●"),
                                        )
                                    }),
                                )
                                .child(
                                    Button::new(("terminal-tab-close", close_id))
                                        .ghost()
                                        .compact()
                                        .xsmall()
                                        .icon(
                                            Icon::new(IconName::Close)
                                                .text_color(cx.theme().muted_foreground),
                                        )
                                        .tooltip(crate::tr!("terminal.close_tab"))
                                        .on_click(move |_, _, cx| {
                                            _ = drawer.update(cx, |this, cx| {
                                                this.workspace_store.update(cx, |store, cx| {
                                                    store.close_terminal_for(
                                                        this.session_id.clone().unwrap_or_default(),
                                                        close_id,
                                                        cx,
                                                    )
                                                });
                                            });
                                        }),
                                )
                                .into_any_element()
                        }
                    })
                    .context_menu({
                        let can_split = self
                            .workspace_store
                            .read(cx)
                            .session_status_for(self.session_id.as_deref(), cx)
                            .is_some_and(|status| status.terminal_split_available);
                        move |menu, _, _| {
                            let item = |kind| Box::new(TerminalTabMenu { id, kind });
                            menu.menu(
                                crate::tr!("terminal.restart").into_owned(),
                                item(TerminalTabMenuKind::Restart),
                            )
                            .menu_with_enable(
                                crate::tr!("terminal.split_horizontal").into_owned(),
                                item(TerminalTabMenuKind::SplitHorizontal),
                                can_split,
                            )
                            .menu_with_enable(
                                crate::tr!("terminal.split_vertical").into_owned(),
                                item(TerminalTabMenuKind::SplitVertical),
                                can_split,
                            )
                            .separator()
                            .menu(
                                crate::tr!("terminal.close_tab").into_owned(),
                                item(TerminalTabMenuKind::Close),
                            )
                        }
                    }),
            );
        }
        tab_strip
    }

    pub(super) fn render_toolbar(
        &self,
        tab_strip: TabBar,
        tabs: &[(u64, String, bool, bool)],
        active_id: Option<u64>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_base::Toolbar {
        let at_limit = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx)
            .is_some_and(|status| status.terminal_limit_reached);
        let can_split = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx)
            .is_some_and(|status| status.terminal_split_available);
        let active_exited = tabs
            .iter()
            .any(|(id, _, exited, _)| Some(*id) == active_id && *exited);
        // A compact page gives the drawer one toolbar row on the page inset:
        // the tab strip, a new-terminal target, and everything else in an
        // overflow menu rather than five dense controls fighting for 393pt.
        let new_tooltip = if at_limit {
            crate::tr!("terminal.max_reached", count = MAX_TERMINALS_PER_SESSION)
        } else {
            crate::tr!("terminal.new")
        };
        let mut header = material::bar_divider_below(
            material::bar("terminal-toolbar", material::Bar::Toolbar, compact, cx),
            cx,
        )
        .child(tab_strip);
        if compact {
            header = header
                .child(
                    material::toolbar_icon_button(
                        "terminal-new",
                        IconName::Plus,
                        new_tooltip,
                        true,
                    )
                    .flex_none()
                    .disabled(at_limit)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store.update(cx, |store, _cx| {
                            store.new_terminal_for(this.session_id.clone().unwrap_or_default())
                        });
                    })),
                )
                .child(
                    material::toolbar_icon_button(
                        "terminal-overflow",
                        IconName::Ellipsis,
                        crate::tr!("mobile.more_actions"),
                        true,
                    )
                    .flex_none()
                    .dropdown_menu(move |menu, _, _| {
                        menu.menu_with_enable(
                            crate::tr!("terminal.split_horizontal").into_owned(),
                            Box::new(TerminalOverflow::SplitHorizontal),
                            can_split,
                        )
                        .menu_with_enable(
                            crate::tr!("terminal.split_vertical").into_owned(),
                            Box::new(TerminalOverflow::SplitVertical),
                            can_split,
                        )
                        .menu_with_enable(
                            crate::tr!("terminal.restart").into_owned(),
                            Box::new(TerminalOverflow::Restart),
                            active_exited,
                        )
                        .separator()
                        .menu(
                            crate::tr!("terminal.close").into_owned(),
                            Box::new(TerminalOverflow::Close),
                        )
                    }),
                );
        } else {
            let gap = cx.theme().tokens.spacing.xs;
            let restart = crate::motion::shown(
                "terminal-restart-shown",
                active_exited.then_some(()),
                SHOW,
                window,
                cx,
            );
            header = header
                .when_some(restart, |this, shown| {
                    this.child(
                        shown.fold_across_gap(
                            gap,
                            Button::new("terminal-restart")
                                .flex_none()
                                .ghost()
                                .small()
                                .label(crate::tr!("terminal.restart"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.workspace_store.update(cx, |store, _cx| {
                                        store.restart_terminal_for(
                                            this.session_id.clone().unwrap_or_default(),
                                        )
                                    });
                                })),
                        ),
                    )
                })
                .child(
                    Button::new("terminal-split-horizontal")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("↔")
                        .disabled(!can_split)
                        .tooltip(crate::tr!("terminal.split_horizontal"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.split_terminal_for(
                                    this.session_id.clone().unwrap_or_default(),
                                    TerminalSplitDirection::Horizontal,
                                )
                            });
                        })),
                )
                .child(
                    Button::new("terminal-split-vertical")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("↕")
                        .disabled(!can_split)
                        .tooltip(crate::tr!("terminal.split_vertical"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.split_terminal_for(
                                    this.session_id.clone().unwrap_or_default(),
                                    TerminalSplitDirection::Vertical,
                                )
                            });
                        })),
                )
                .child(
                    Button::new("terminal-new")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("+")
                        .disabled(at_limit)
                        .tooltip(new_tooltip)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.new_terminal_for(this.session_id.clone().unwrap_or_default())
                            });
                        })),
                )
                .child(
                    material::toolbar_icon_button(
                        "terminal-close-drawer",
                        IconName::Close,
                        crate::tr!("terminal.close"),
                        false,
                    )
                    .flex_none()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store.update(cx, |store, cx| {
                            store.close_terminal_panel_for(
                                this.session_id.clone().unwrap_or_default(),
                                cx,
                            )
                        });
                    })),
                );
        }
        header
    }
}
