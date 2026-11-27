use super::*;

impl SettingsPage {
    pub(super) fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.host_permissions = None;
        if !self.section_applies(&section, cx) {
            self.section = Section::General;
            self.sync_holds(cx);
            Self::return_to_settings_root(&self.window_state, cx);
            cx.notify();
            return;
        }
        self.section = section;
        self.sync_holds(cx);
        // Arriving from a deep link or the palette must not leave the selected
        // section hidden inside a folded subgroup.
        if section.advanced() {
            self.advanced_expanded = Some(true);
        }
        // Which section is open is this page's business; *that* a detail is
        // open is navigation, and belongs to the window's one history.
        self.window_state.update(cx, |state, cx| {
            if state.compact {
                state.go(crate::window_state::Destination::SettingsSection, cx);
            }
        });
        cx.notify();
    }

    /// The open section's name, which is also this page's compact nav-bar
    /// title and the label its child's Back control carries.
    pub(crate) fn section_title(&self, cx: &App) -> SharedString {
        if self.section == Section::ProjectThreadRules
            && self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsThreadRules
        {
            return Section::Threads.label();
        }
        if self.section == Section::SourceControlHost {
            return match &self.source_control_host {
                Some(editor)
                    if self.window_state.read(cx).destination()
                        == crate::window_state::Destination::SettingsSourceControlHost =>
                {
                    editor.read(cx).host.clone().into()
                }
                _ => Section::SourceControl.label(),
            };
        }
        // The phone page names just the project: the nav bar has no room for
        // the dialog's sentence-long title.
        if self.section == Section::ProjectThreadRules
            && let Some(editor) = &self.project_rules_editor
        {
            return editor.read(cx).project.name.clone().into();
        }
        if self.section == Section::Harness {
            if self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsHarness
            {
                return Section::Providers.label();
            }
            if let Some(page) = &self.harness_page {
                return crate::harness_dialog::harness_name(
                    self.store.read(cx),
                    page.read(cx).harness(),
                )
                .into();
            }
        }
        self.section.label()
    }

    pub(super) fn nav_item(&self, section: Section, cx: &mut Context<Self>) -> TabItem {
        TabItem::new(section.id(), section.label())
            .icon(section.icon())
            .selected(self.section == section)
            .on_click(cx.listener(move |this, _, _, cx| this.select_section(section, cx)))
    }

    pub(super) fn group_caption(&self, group: SectionGroup, cx: &mut Context<Self>) -> AnyElement {
        let (label, selector) = match group {
            SectionGroup::Device => (
                crate::tr!("settings.this_device").into_owned(),
                "settings-device-caption",
            ),
            SectionGroup::Machine if self.settings_hosts(cx).len() > 1 => {
                return self.host_selector(cx);
            }
            SectionGroup::Machine => {
                let name = crate::machines::own_name(&self.store, cx);
                // A member manages none of the machine's settings; what this
                // group holds for it is the space's own threads.
                let label = self.store.read(cx).space_label().unwrap_or_else(|| {
                    name.map_or_else(
                        || crate::tr!("settings.machine_settings_fallback").into_owned(),
                        |name| crate::tr!("settings.machine_settings", name = name).into_owned(),
                    )
                });
                (label, "settings-machine-caption")
            }
        };
        crate::material::group_caption(label, cx)
            .debug_selector(move || selector.into())
            .into_any_element()
    }

    /// The machine group's caption over several hosts: it names the host
    /// whose settings the group holds and chooses another.
    pub(super) fn host_selector(&self, cx: &mut Context<Self>) -> AnyElement {
        let shown = self.store.read(cx).host_key();
        let options = self
            .settings_hosts(cx)
            .into_iter()
            .map(|store| {
                let label = crate::machines::host_title(store, cx);
                let host = store.read(cx).host_key();
                SelectRowOption {
                    id: format!("settings-host-{}", host.name()).into(),
                    label,
                    description: None,
                    selected: host == shown,
                    value: host,
                }
            })
            .collect();
        let label = crate::tr!(
            "settings.machine_settings",
            name = crate::machines::host_title(&self.store, cx)
        );
        let trigger = Button::new("settings-host-selector")
            .debug_selector(|| "settings-host-selector".into())
            .text()
            .xsmall()
            .aria_label(crate::tr!("settings.choose_host"))
            .child(
                gpui_base::h_flex()
                    .gap_1()
                    .text_caption(cx)
                    .child(crate::widgets::fade_text(label))
                    .child(
                        Icon::new(IconName::ChevronDown).text_color(cx.theme().muted_foreground),
                    ),
            );
        gpui_base::h_flex()
            .pl(px(crate::material::CARD_INSET))
            .pb(cx.theme().tokens.spacing.sm)
            .child(crate::settings_rows::select_menu(
                trigger,
                "settings-host-popover",
                "settings-host-menu",
                200.,
                crate::tr!("settings.choose_host").into_owned().into(),
                options,
                self.window_state.read(cx).compact,
                |host, _, cx| crate::shell::choose_settings_host(host, cx),
                cx,
            ))
            .into_any_element()
    }

    /// Draw the rail at `opacity` from this frame on.
    pub(crate) fn fade_rail(this: &Entity<Self>, opacity: f32, cx: &mut App) {
        if this.read(cx).rail_opacity != opacity {
            this.update(cx, |page, _| page.rail_opacity = opacity);
        }
    }

    pub(super) fn advanced_expanded(&self) -> bool {
        self.advanced_expanded
            .unwrap_or(!self.capabilities.folds_advanced())
    }

    /// The **Advanced** subgroup's disclosure, shared by the rail and the
    /// compact section list: the same caption style as a group's, plus the
    /// chevron that opens it.
    pub(super) fn advanced_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let spacing = &cx.theme().tokens.spacing;
        // A row, so the button is as wide as its label, not the rail.
        gpui_base::h_flex()
            .pl(px(crate::material::CARD_INSET))
            .pt(spacing.sm)
            .pb(spacing.sm)
            .child(AdvancedToggle {
                expanded: self.advanced_expanded(),
                on_click: Rc::new(cx.listener(|this, _, _, cx| {
                    this.advanced_expanded = Some(!this.advanced_expanded());
                    cx.notify();
                })),
            })
            .into_any_element()
    }

    /// The wide rail's way out of Settings. Compact has no such row: there, the
    /// shell's nav bar carries Back like it does on every other page.
    pub(super) fn back_row(&self, cx: &mut Context<Self>) -> AnyElement {
        crate::material::bar(
            "settings-back-bar",
            crate::material::Bar::Toolbar,
            false,
            cx,
        )
        .child(
            crate::material::selectable_row(
                crate::material::accessible_clickable(
                    gpui_base::h_flex(),
                    "settings-back",
                    Role::Button,
                    crate::tr!("settings.back"),
                    cx,
                ),
                false,
                false,
                cx,
            )
            .flex_1()
            .text_body(false, cx)
            .text_color(cx.theme().sidebar_foreground)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.close_settings(cx));
            }))
            .child(Icon::new(IconName::ArrowLeft).text_color(cx.theme().muted_foreground))
            .child(crate::widgets::fade_text(crate::tr!("settings.back"))),
        )
        .into_any_element()
    }

    pub(super) fn render_nav(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let spacing = cx.theme().tokens.spacing;
        // One selection over the device, machine and advanced rails: its
        // fill glides from a row of one to a row of another.
        let mark = Rc::new(crate::widgets::tabs::Glide::new(
            &"settings-rail".into(),
            Some(crate::widgets::tabs::item_key(&self.section.id().into())),
            window,
            cx,
        ));
        let mut tabs = v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .px(spacing.sm)
            .gap(spacing.xxs)
            .child(mark.rail(cx));
        for (index, group) in [SectionGroup::Device, SectionGroup::Machine]
            .into_iter()
            .enumerate()
        {
            let sections = SECTIONS
                .into_iter()
                .filter(|section| section.group() == group && self.section_applies(section, cx))
                .collect::<Vec<_>>();
            if sections.is_empty() {
                continue;
            }
            tabs = tabs.child(
                div()
                    .when(index > 0, |caption| caption.pt(spacing.md))
                    .child(self.group_caption(group, cx)),
            );
            let plain = sections
                .iter()
                .copied()
                .filter(|section| !section.advanced())
                .map(|section| self.nav_item(section, cx))
                .collect::<Vec<_>>();
            tabs = tabs.child(
                TabBar::new(if group == SectionGroup::Device {
                    "settings-nav-tabs"
                } else {
                    "settings-machine-tabs"
                })
                .rail()
                .shared_mark(mark.clone())
                .aria_label(crate::tr!("settings.title"))
                .items(plain),
            );
            if sections.iter().any(|section| section.advanced()) {
                tabs = tabs.child(self.advanced_toggle(cx)).child(
                    crate::widgets::Collapsible::new(
                        "settings-advanced-tabs-fold",
                        self.advanced_expanded(),
                    )
                    .motion(&self.motion)
                    .w_full()
                    .content(
                        TabBar::new("settings-advanced-tabs")
                            .rail()
                            .shared_mark(mark.clone())
                            .aria_label(crate::tr!("settings.advanced"))
                            .items(
                                sections
                                    .into_iter()
                                    .filter(|section| section.advanced())
                                    .map(|section| self.nav_item(section, cx))
                                    .collect::<Vec<_>>(),
                            ),
                    ),
                );
            }
        }
        // Settings replaces the whole window, so this rail is the window's
        // left column: it carries the wordmark and clears the platform's
        // own window controls, exactly as the workspace sidebar does, and
        // keeps its width beside a side cutout.
        let seam = crate::window_seam::insets_along(
            "settings-rail-seam",
            gpui::Edges {
                right: false,
                ..gpui::Edges::all(true)
            },
            window,
            cx,
        );
        v_flex()
            .flex_none()
            .w(px(NAV_WIDTH) + seam.left)
            .h_full()
            .pt(seam.top)
            .pb(seam.bottom)
            .pl(seam.left)
            .opacity(self.rail_opacity)
            .bg(cx.theme().sidebar)
            .child(
                window_drag_area(
                    "settings-nav-drag",
                    crate::material::bar(
                        "settings-nav-strip",
                        crate::material::Bar::Strip,
                        false,
                        cx,
                    )
                    .pl(window_caption::traffic_light_inset(window, cx).max(spacing.sm)),
                    window,
                    cx,
                )
                .child(crate::material::brand_wordmark(cx)),
            )
            .child(tabs)
            .child(div().flex_none().child(self.back_row(cx)))
            .into_any_element()
    }

    /// Compact clients have no room for a 255px rail beside the content, so the
    /// same sections become a full-width list that pushes to a detail page.
    /// The page draws no header and no back row: in compact every page wears
    /// the shell's one nav bar (`crate::shell`).
    pub(crate) fn render_compact_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let advanced_expanded = self.advanced_expanded();
        let mut column = v_flex()
            .w_full()
            .p(px(crate::material::COMPACT_PAGE_INSET))
            .gap(crate::material::section_gap(cx));
        for group in [SectionGroup::Device, SectionGroup::Machine] {
            let rows = |advanced: bool, cx: &mut Context<Self>| {
                SECTIONS
                    .into_iter()
                    .filter(|section| {
                        section.group() == group
                            && section.advanced() == advanced
                            && self.section_applies(section, cx)
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|section| self.compact_section_row(section, cx))
                    .collect::<Vec<_>>()
            };
            let plain = rows(false, cx);
            let advanced = rows(true, cx);
            if plain.is_empty() && advanced.is_empty() {
                continue;
            }
            let mut card = v_flex().w_full().child(self.group_caption(group, cx));
            if !plain.is_empty() {
                card = card.child(crate::material::grouped(plain, cx));
            }
            if !advanced.is_empty() {
                card = card.child(self.advanced_toggle(cx)).child(
                    crate::widgets::Collapsible::new(
                        "settings-advanced-list-fold",
                        advanced_expanded,
                    )
                    .motion(&self.motion)
                    .w_full()
                    .content(crate::material::grouped(advanced, cx)),
                );
            }
            column = column.child(card);
        }
        crate::scroll::page_viewport(
            "settings-section-bounce",
            &self.section_scroll,
            div()
                .id("settings-section-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(&self.section_scroll)
                .child(column),
        )
        .into_any_element()
    }

    /// One tappable section row of the compact list.
    pub(super) fn compact_section_row(
        &self,
        section: Section,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = section.label();
        crate::material::list_row(section.id(), label.clone(), cx)
            .debug_selector(move || section.id().into())
            .text_body(true, cx)
            .child(Icon::new(section.icon()).text_color(cx.theme().muted_foreground))
            .child(div().flex_1().child(crate::widgets::fade_text(label)))
            .child(Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground))
            .on_click(cx.listener(move |this, _, _, cx| this.select_section(section, cx)))
            .into_any_element()
    }
}
