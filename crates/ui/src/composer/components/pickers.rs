use super::super::*;

#[derive(Clone)]
/// One selectable model in the picker (a catalog [`ModelSpec`] row).
struct ModelRow {
    /// Provider-native model id (the favorites key + selection value), or the
    /// ACP agent's registry id when `acp` is set.
    id: String,
    /// Display name.
    name: String,
    provider: ProviderKind,
    /// Which provider profile this row belongs to (`None` = the built-in profile
    /// for `provider`). Lets a native provider expose several profiles — e.g.
    /// official Claude and a third-party endpoint — in one rail.
    profile_id: Option<String>,
    /// This row starts a session with an installed ACP agent rather than
    /// selecting a model (ACP agents own their model list).
    acp: bool,
    favorite: bool,
}

/// The provider glyph tinted with the accent configured on its Settings →
/// Providers card, falling back to the provider's own brand tint.
fn tinted_provider_glyph(provider: ProviderKind, store: &WorkspaceStore) -> Icon {
    let glyph = provider_glyph(provider);
    let profile_id = tcode_core::settings::Settings::builtin_profile_id(provider);
    match store.provider_profile_accent(profile_id) {
        Some(accent) => glyph.text_color(rgb(accent)),
        None => glyph,
    }
}

/// A profile's rail glyph: the kind's glyph tinted with the profile's own accent
/// (so a third-party profile can be told apart from the built-in at a glance).
fn tinted_profile_glyph(profile_id: &str, store: &WorkspaceStore) -> Icon {
    let glyph = provider_glyph(store.provider_profile_kind(profile_id));
    match store.provider_profile_accent(profile_id) {
        Some(accent) => glyph.text_color(rgb(accent)),
        None => glyph,
    }
}

impl Composer {
    /// The rail the picker shows: an explicit user choice, else Favorites when
    /// any favorites exist (S1 §2), else the active session's profile.
    pub(in super::super) fn rail_for(
        &self,
        provider: ProviderKind,
        agent_id: Option<&str>,
        profile_id: Option<&str>,
        has_favorites: bool,
    ) -> PickerRail {
        if let Some(rail) = self.picker_rail.clone() {
            return rail;
        }
        match (provider, agent_id) {
            (ProviderKind::Acp, Some(id)) => PickerRail::Acp(id.to_string()),
            _ if has_favorites => PickerRail::Favorites,
            // A native session's rail is its selected profile, defaulting to the
            // built-in profile for the provider.
            _ => PickerRail::Profile(profile_id.map(str::to_string).unwrap_or_else(|| {
                tcode_core::settings::Settings::builtin_profile_id(provider).to_string()
            })),
        }
    }

    /// The model-picker button + popover (anchored above, ~360px).
    pub(in super::super) fn render_model_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.workspace_store.read(cx);
        let composer_state = store.composer_state();
        let Some(active_model) = composer_state.active_model.clone() else {
            return div().into_any_element();
        };
        let provider = active_model.provider;
        let current_model = active_model.model;
        let acp_agent_id = active_model.acp_agent_id;
        let active_profile = active_model.profile_id;
        let catalog = store.provider_model_catalog(provider);
        // The picker honors the provider card's Models section: hidden models
        // are gone, custom slugs are present, and the persisted order (plus
        // favorites-first) decides the sequence. When a third-party profile is
        // active, resolve against *its* card so its custom models are named.
        let resolved = match &active_profile {
            Some(id) => store.picker_models_for_profile(id),
            None => composer_state.picker_models(provider),
        };
        let display = current_model_name_resolved(&resolved, &catalog, current_model.as_deref());

        // Build the filtered row list for the current frame. Favorites open
        // first when any exist (S1 §2). The favorites sweep covers every
        // enabled profile — built-ins *and* third-party (e.g. a Kimi endpoint)
        // — in rail order, so a starred custom-profile model is not lost.
        let query = self.model_search.read(cx).value().to_lowercase();
        let fav_profiles: Vec<(String, ProviderKind)> = {
            let profiles = store.enabled_profiles();
            PICKER_PROVIDER_KINDS
                .into_iter()
                .flat_map(|kind| {
                    profiles
                        .iter()
                        .filter(move |profile| profile.kind == kind)
                        .map(|profile| (profile.id.clone(), profile.kind))
                })
                .collect()
        };
        let has_favorites = fav_profiles.iter().any(|(id, _)| {
            store
                .picker_models_for_profile(id)
                .iter()
                .any(|m| m.favorite)
        });
        let rail = self.rail_for(
            provider,
            acp_agent_id.as_deref(),
            active_profile.as_deref(),
            has_favorites,
        );
        let all_rows: Vec<ModelRow> = match &rail {
            PickerRail::Favorites => fav_profiles
                .iter()
                .flat_map(|(id, kind)| {
                    let is_builtin = tcode_core::settings::Settings::is_builtin_profile_id(id);
                    let profile_id = (!is_builtin).then(|| id.clone());
                    let kind = *kind;
                    store
                        .picker_models_for_profile(id)
                        .into_iter()
                        .filter(|m| m.favorite)
                        .map(move |m| ModelRow {
                            id: m.id,
                            name: m.name,
                            provider: kind,
                            profile_id: profile_id.clone(),
                            acp: false,
                            favorite: true,
                        })
                })
                .collect(),
            // Each profile is its own rail and lists only its own models: the
            // built-in profiles show the official catalog; a third-party profile
            // (e.g. Klaude Kode → Kimi) shows only the models added to its card.
            PickerRail::Profile(id) => {
                let kind = store.provider_profile_kind(id);
                let is_builtin = tcode_core::settings::Settings::is_builtin_profile_id(id);
                let profile_id = id.clone();
                store
                    .picker_models_for_profile(id)
                    .into_iter()
                    .map(move |m| ModelRow {
                        id: m.id,
                        name: m.name,
                        provider: kind,
                        profile_id: (!is_builtin).then(|| profile_id.clone()),
                        acp: false,
                        favorite: m.favorite,
                    })
                    .collect()
            }
            // One row: "use this agent". Its models arrive as ProviderOptions
            // once the session starts and render in the traits picker.
            PickerRail::Acp(id) => store
                .installed_acp_agent(id)
                .into_iter()
                .map(|agent| ModelRow {
                    id: agent.id,
                    name: agent.name,
                    provider: ProviderKind::Acp,
                    profile_id: None,
                    acp: true,
                    favorite: false,
                })
                .collect(),
        };
        let rows: Vec<ModelRow> = all_rows
            .into_iter()
            .filter(|r| query.is_empty() || r.name.to_lowercase().contains(&query))
            .collect();
        // Only the built-in profiles have a probed catalog that can still be
        // loading; a third-party profile shows its own slugs immediately.
        let loading = composer_state.models_loading(provider)
            && matches!(&rail, PickerRail::Profile(id) if tcode_core::settings::Settings::is_builtin_profile_id(id))
            && rows.is_empty()
            && query.is_empty();

        let composer = cx.entity();
        let store_entity = self.workspace_store.clone();
        let model_search = self.model_search.clone();
        let pending_restart = composer_state.model_pending_restart;
        // On an ACP rail the "selected" row is the agent itself.
        let selected = match provider {
            ProviderKind::Acp => acp_agent_id.clone(),
            _ => current_model.clone(),
        };
        let acp_rail_agents: Vec<(String, String)> = store
            .settings_installed_acp_agents()
            .into_iter()
            .filter(|agent| agent.enabled)
            .map(|agent| (agent.id.clone(), agent.name.clone()))
            .collect();

        let trigger = Button::new("model-picker")
            .ghost()
            .compact()
            .h(px(28.))
            .rounded(crate::material::radius_input())
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .child(tinted_provider_glyph(provider, store).small())
                    .child(div().font_medium().child(display))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );

        crate::material::overlay_popover(("model-picker-popover", self.model_picker_token))
            .anchor(Anchor::BottomLeft)
            .default_open(self.model_picker_token > 0)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let rows = rows.clone();
                let store_entity = store_entity.clone();
                let model_search = model_search.clone();
                let composer = composer.clone();
                let selected = selected.clone();
                let popover = cx.entity();
                let rail = rail.clone();
                let acp_rail_agents = acp_rail_agents.clone();
                render_model_pane(
                    &rows,
                    &selected,
                    rail,
                    &acp_rail_agents,
                    pending_restart,
                    loading,
                    &store_entity,
                    &model_search,
                    &composer,
                    &popover,
                    cx,
                )
            })
            .into_any_element()
    }

    /// The traits chip ("High · 200k") + descriptor popover. Empty element when
    /// the current model has no descriptors.
    pub(in super::super) fn render_traits_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.workspace_store.read(cx);
        let composer = store.composer_state();
        // Native providers describe their options through the model catalog; ACP
        // agents push theirs over the wire (`AgentEvent::ProviderOptions`). Both
        // arrive as `OptionDescriptor`s and render through this one picker.
        let descriptors = composer.active_option_descriptors.clone();
        if descriptors.is_empty() {
            return div().into_any_element();
        }
        let spec = match composer.active_model_spec.clone() {
            Some(spec) => spec,
            None => ModelSpec {
                id: String::new(),
                display_name: String::new(),
                is_default: false,
                options: descriptors,
            },
        };
        let selections = composer.active_option_selections;
        let ultrathink_armed = composer.ultrathink_armed;
        let Some(label) = traits_chip_label(&spec, &selections, ultrathink_armed) else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        // The reasoning section is locked while the prompt text itself contains
        // "ultrathink" (T3).
        let locked = self
            .input
            .read(cx)
            .value()
            .to_lowercase()
            .contains("ultrathink");
        let pending_restart = composer.options_pending_restart;

        let trigger = Button::new("traits-chip")
            .ghost()
            .compact()
            .h(px(28.))
            .rounded(crate::material::radius_chip())
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(label)
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );

        let store_entity = self.workspace_store.clone();
        let composer_entity = cx.entity();
        let context_window_custom = self.context_window_custom.clone();
        crate::material::overlay_popover("traits-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                let popover = cx.entity();
                composer_entity.update(cx, |composer, _cx| {
                    composer.traits_popover = Some(popover.clone());
                });
                let context_window_custom_error =
                    composer_entity.read(cx).context_window_custom_error;
                render_traits_pane(
                    &spec,
                    &selections,
                    ultrathink_armed,
                    locked,
                    pending_restart,
                    &store_entity,
                    &context_window_custom,
                    context_window_custom_error,
                    &popover,
                    cx,
                )
            })
            .into_any_element()
    }

    /// The Build/Plan interaction-mode chip (S1 §4).
    pub(in super::super) fn render_mode_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self
            .workspace_store
            .read(cx)
            .composer_state()
            .interaction_mode;
        let muted = cx.theme().muted_foreground;
        let (icon, label, tooltip) = match mode {
            InteractionMode::Build => (
                "icons/box.svg",
                crate::tr!("composer.build"),
                crate::tr!("composer.build_tooltip"),
            ),
            InteractionMode::Plan => (
                "icons/ruler.svg",
                crate::tr!("composer.plan"),
                crate::tr!("composer.plan_tooltip"),
            ),
        };
        Button::new("mode-chip")
            .ghost()
            .compact()
            .h(px(28.))
            .rounded(crate::material::radius_chip())
            .tooltip(tooltip)
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(11.5))
                    .text_color(muted)
                    .child(Icon::empty().path(icon).small().text_color(muted))
                    .child(label),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.workspace_store
                    .update(cx, |store, _cx| store.toggle_interaction_mode());
            }))
            .into_any_element()
    }

    /// The circular context-window meter (ring showing used%, red > 90%) + a
    /// hover/click popover (T3's `ContextWindowMeter`).
    pub(in super::super) fn render_context_meter(&self, cx: &mut Context<Self>) -> AnyElement {
        let composer = self.workspace_store.read(cx).composer_state();
        let usage = composer.token_usage;
        let account_usage = composer.usage.clone();
        let provider = composer.provider;
        let pct = usage.and_then(|u| context_meter::used_percentage(&u));
        let overloaded = pct.map(context_meter::is_overloaded).unwrap_or(false);
        let ring_color: Hsla = if overloaded {
            rgb(METER_RED).into()
        } else {
            rgb(METER_BLUE).into()
        };
        let mut track = cx.theme().muted_foreground;
        track.a = 0.35;

        let trigger = Button::new("context-meter")
            .ghost()
            .compact()
            .h(px(28.))
            .rounded(crate::material::radius_chip())
            .child(div().size(px(16.)).child(crate::widgets::ring::ring_canvas(
                pct.unwrap_or(0.0),
                ring_color,
                track,
            )));

        crate::material::overlay_popover("context-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_context_meter_pane(usage, account_usage.clone(), provider, pct, cx)
            })
            .into_any_element()
    }

    /// The approval-mode selector: a chip showing the current mode (icon +
    /// label) opening a popover of the three modes (icon + bold name + muted
    /// description, ✓ on the current one).
    pub(in super::super) fn render_permission_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let composer = self.workspace_store.read(cx).composer_state();
        let current = composer.approval_mode;
        let native_approval_modes_enabled = composer.native_approval_modes_enabled;
        let (label, icon_path) = approval_mode_meta(current);
        let muted = cx.theme().muted_foreground;

        let trigger = Button::new("permission-chip")
            .ghost()
            .compact()
            .h(px(28.))
            .rounded(crate::material::radius_input())
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(Icon::empty().path(icon_path).small().text_color(muted))
                    .child(label)
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );

        let store_entity = self.workspace_store.clone();
        let pending_restart = composer.approval_pending_restart;
        crate::material::overlay_popover("permission-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_permission_pane(
                    current,
                    pending_restart,
                    native_approval_modes_enabled,
                    &store_entity,
                    &cx.entity(),
                    cx,
                )
            })
            .into_any_element()
    }

    /// The "⋯" overflow button + popover holding the context / permission /
    /// mode controls when the control row is too narrow to show them inline.
    pub(in super::super) fn render_overflow_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let composer = self.workspace_store.read(cx).composer_state();
        let usage = composer.token_usage;
        let muted = cx.theme().muted_foreground;
        let mode = composer.approval_mode;
        let interaction = composer.interaction_mode;
        let store_entity = self.workspace_store.clone();

        let trigger = Button::new("overflow-controls")
            .ghost()
            .compact()
            .tooltip(crate::tr!("composer.more_controls"))
            .child(Icon::new(IconName::Ellipsis).small().text_color(muted));

        crate::material::overlay_popover("overflow-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_overflow_pane(usage, mode, interaction, &store_entity, &cx.entity(), cx)
            })
            .into_any_element()
    }
}

#[allow(clippy::too_many_arguments)]
fn render_model_pane(
    rows: &[ModelRow],
    selected: &Option<String>,
    rail: PickerRail,
    // (id, name) of every installed+enabled ACP agent — one rail entry each.
    acp_agents: &[(String, String)],
    pending_restart: bool,
    loading: bool,
    store_entity: &Entity<WorkspaceStore>,
    model_search: &Entity<InputState>,
    composer: &Entity<Composer>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;

    // Left rail: favorites star + one glyph per profile. The `label` names the
    // entry on hover so two profiles of the same kind (official vs third-party)
    // are told apart even though they share a glyph.
    let rail_icon = |id: gpui::SharedString,
                     label: gpui::SharedString,
                     icon: Icon,
                     active: bool,
                     target: PickerRail,
                     cx: &mut Context<PopoverState>|
     -> AnyElement {
        let composer = composer.clone();
        crate::material::accessible_clickable(div(), id, Role::Tab, label.clone(), cx)
            .aria_selected(active)
            .flex_none()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .when(active, |s| s.bg(cx.theme().muted))
            .hover(|s| s.bg(cx.theme().muted))
            .tooltip(move |window, cx| {
                crate::widgets::tooltip::Tooltip::new(label.clone()).build(window, cx)
            })
            .child(
                icon.small()
                    .text_color(if active { cx.theme().foreground } else { muted }),
            )
            .on_click(move |_, _, cx| {
                let target = target.clone();
                composer.update(cx, |c, cx| {
                    c.picker_rail = Some(target);
                    cx.notify();
                });
            })
            .into_any_element()
    };

    let mut rail_col = v_flex().w_full().py_2().px_1p5().gap_1().child(rail_icon(
        "rail-fav".into(),
        crate::tr!("composer.favorites").into_owned().into(),
        Icon::new(IconName::Star),
        rail == PickerRail::Favorites,
        PickerRail::Favorites,
        cx,
    ));
    // One entry per *enabled* native profile: every built-in plus any
    // user-created profiles whose switch is on. Each is its own rail.
    let profile_ids: Vec<String> = {
        let store = store_entity.read(cx);
        let profiles = store.enabled_profiles();
        PICKER_PROVIDER_KINDS
            .into_iter()
            .flat_map(|kind| {
                profiles
                    .iter()
                    .filter(move |profile| profile.kind == kind)
                    .map(|profile| profile.id.clone())
            })
            .collect()
    };
    for id in profile_ids {
        let glyph = tinted_profile_glyph(&id, store_entity.read(cx));
        let label = store_entity.read(cx).provider_profile_display_name(&id);
        rail_col = rail_col.child(rail_icon(
            gpui::SharedString::from(format!("rail-profile-{id}")),
            label.into(),
            glyph,
            rail == PickerRail::Profile(id.clone()),
            PickerRail::Profile(id.clone()),
            cx,
        ));
    }
    // …then one entry per installed ACP agent (Settings → Providers → ACP Agents).
    for (id, name) in acp_agents {
        rail_col = rail_col.child(rail_icon(
            gpui::SharedString::from(format!("rail-acp-{id}")),
            gpui::SharedString::from(name.clone()),
            Icon::empty().path("icons/box.svg"),
            rail == PickerRail::Acp(id.clone()),
            PickerRail::Acp(id.clone()),
            cx,
        ));
    }
    let rail = div()
        .id("model-provider-rail")
        .role(Role::TabList)
        .aria_label(crate::tr!("composer.model_sources"))
        .flex_none()
        .w(px(44.))
        .h_full()
        .border_r_1()
        .border_color(cx.theme().border)
        .overflow_y_scroll()
        .child(rail_col);

    // Main pane: search + rows.
    let mut list = v_flex().w_full().min_h_0().gap_0p5().px_1().py_1();
    for (index, row) in rows.iter().enumerate() {
        list = list.child(render_model_row(
            row,
            index,
            selected,
            store_entity,
            popover,
            cx,
        ));
    }
    if rows.is_empty() {
        list = list.child(
            div()
                .flex_none()
                .px_3()
                .py_4()
                .text_size(px(13.))
                .text_color(muted)
                .child(if loading {
                    crate::tr!("composer.loading_models")
                } else {
                    crate::tr!("composer.no_models")
                }),
        );
    }

    let mut pane = v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(Input::new(model_search).appearance(false)),
        )
        .child(
            div()
                .id("model-picker-list")
                .role(Role::ListBox)
                .aria_label(crate::tr!("composer.model_results"))
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(list),
        );
    if pending_restart {
        pane = pane.child(
            div()
                .px_3()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!("composer.restart_note")),
        );
    }

    // Secondary+1-9 selects the corresponding row while the popover is open.
    let key_rows: Vec<ModelRow> = rows.iter().take(9).cloned().collect();
    let store_key = store_entity.clone();
    let popover_key = popover.clone();

    h_flex()
        .w(px(360.))
        .h(px(360.))
        .items_stretch()
        .rounded(crate::material::radius_card())
        .overflow_hidden()
        .on_key_down(move |ev, window, cx| {
            if !ev.keystroke.modifiers.secondary() {
                return;
            }
            if let Ok(n) = ev.keystroke.key.parse::<usize>()
                && n >= 1
                && n <= key_rows.len()
            {
                let row = key_rows[n - 1].clone();
                store_key.update(cx, |store, _cx| {
                    if row.acp {
                        store.set_active_acp_agent(row.id);
                    } else {
                        store.set_active_model(row.provider, Some(row.id), row.profile_id);
                    }
                });
                popover_key.update(cx, |st, cx| st.dismiss(window, cx));
            }
        })
        .child(rail)
        .child(pane)
        .with_animation(
            "model-picker-pop-in",
            Animation::new(Duration::from_millis(150)),
            |element, delta| element.opacity(delta),
        )
        .into_any_element()
}

fn render_model_row(
    row: &ModelRow,
    index: usize,
    selected: &Option<String>,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let is_current = selected.as_deref() == Some(row.id.as_str());
    let is_acp = row.acp;
    let is_fav = !is_acp && row.favorite;
    let name = row.name.clone();
    let id = row.id.clone();
    let provider = row.provider;
    let profile_id = row.profile_id.clone();
    let fav_id = row.id.clone();

    let store_select = store_entity.clone();
    let popover_select = popover.clone();
    let store_fav = store_entity.clone();
    let popover_fav = popover.clone();

    let accessible_label = crate::tr!("composer.model_option", model = name.clone()).into_owned();
    h_flex()
        .id(("model-row", index))
        .role(Role::ListBoxOption)
        .aria_label(accessible_label)
        .aria_selected(is_current)
        .when(is_current, |row| row.aria_active_descendant())
        .flex_none()
        .w_full()
        .min_h(px(28.))
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .rounded(crate::material::radius_chip())
        .cursor_pointer()
        .when(is_current, |row| row.bg(cx.theme().list_active))
        .hover(|s| s.bg(cx.theme().muted))
        .on_click(move |_, window, cx| {
            let id = id.clone();
            let profile_id = profile_id.clone();
            store_select.update(cx, |store, _cx| {
                if is_acp {
                    store.set_active_acp_agent(id);
                } else {
                    store.set_active_model(provider, Some(id), profile_id);
                }
            });
            popover_select.update(cx, |st, cx| st.dismiss(window, cx));
        })
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_size(px(13.))
                        .child(div().font_medium().child(name))
                        .when(is_current, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .xsmall()
                                    .text_color(cx.theme().primary),
                            )
                        }),
                )
                .child({
                    // A third-party profile's row is attributed to that profile
                    // (its accent + display name), not the built-in provider.
                    let (glyph, label) = match &row.profile_id {
                        Some(id) => {
                            let store = store_entity.read(cx);
                            (
                                tinted_profile_glyph(id, store),
                                store.provider_profile_display_name(id).into(),
                            )
                        }
                        None => (
                            tinted_provider_glyph(row.provider, store_entity.read(cx)),
                            gpui::SharedString::from(provider_label(row.provider)),
                        ),
                    };
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(glyph.xsmall())
                        .child(label)
                })
                .when(!row.provider.caps().mcp_servers, |this| {
                    this.child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(crate::tr!("providers.mcp_unavailable")),
                    )
                }),
        )
        .when(index < 9, |this| {
            this.child(
                div()
                    .flex_none()
                    .px_1()
                    .py(px(1.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(format_secondary_shortcut(&(index + 1).to_string())),
            )
        })
        .child(
            crate::material::accessible_clickable(
                div(),
                ("model-fav", index),
                Role::Button,
                if is_fav {
                    crate::tr!("composer.remove_favorite")
                } else {
                    crate::tr!("composer.add_favorite")
                },
                cx,
            )
            .flex_none()
            .p(px(2.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().accent))
            .child(
                Icon::new(if is_fav {
                    IconName::StarFill
                } else {
                    IconName::Star
                })
                .xsmall()
                .text_color(if is_fav {
                    rgb(CLAUDE_BRAND_COLOR).into()
                } else {
                    muted
                }),
            )
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let fav_id = fav_id.clone();
                store_fav.update(cx, |store, _cx| store.toggle_favorite_model(fav_id));
                // Refresh the open popover so the star + ordering update.
                popover_fav.update(cx, |_, cx| cx.notify());
            }),
        )
        .into_any_element()
}

/// The approval-mode popover: three rows (icon + bold name + muted
/// description), a ✓ on the current mode, and an optional restart note when the
/// live provider (Codex) will restart to apply the change on the next turn.
fn render_permission_pane(
    current: ApprovalMode,
    pending_restart: bool,
    native_approval_modes_enabled: bool,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let primary = cx.theme().primary;

    let mut list = v_flex()
        .id("permission-menu")
        .role(Role::Menu)
        .aria_label(crate::tr!("approval.choose_mode"))
        .w_full()
        .p_1()
        .gap_0p5();
    for (index, (mode, label, description, icon_path)) in APPROVAL_MODES.iter().enumerate() {
        let mode = *mode;
        let is_current = mode == current;
        let is_disabled = !native_approval_modes_enabled
            && matches!(
                mode,
                ApprovalMode::Supervised | ApprovalMode::AutoAcceptEdits
            );
        let store = store_entity.clone();
        let popover = popover.clone();
        let disabled_hint = crate::tr!("approval.pi_native_approvals_required");
        let accessible_label = crate::tr!(
            "approval.mode_option",
            label = crate::tr!(*label),
            description = if is_disabled {
                format!("{} {}", crate::tr!(*description), disabled_hint)
            } else {
                crate::tr!(*description).into_owned()
            }
        )
        .into_owned();
        list = list.child(
            h_flex()
                .id(("permission-row", index))
                .role(Role::MenuItem)
                .aria_label(accessible_label)
                .aria_selected(is_current)
                .when(is_current, |row| row.aria_active_descendant())
                .w_full()
                .min_h(px(28.))
                .px_2()
                .py_1()
                .gap_2()
                .items_start()
                .rounded(crate::material::radius_chip())
                .when(is_current, |row| row.bg(cx.theme().list_active))
                .when(is_disabled, |row| row.opacity(0.55))
                .when(!is_disabled, |row| {
                    row.cursor_pointer()
                        .hover(|s| s.bg(cx.theme().muted))
                        .on_click(move |_, window, cx| {
                            store.update(cx, |store, _cx| store.set_active_approval_mode(mode));
                            popover.update(cx, |st, cx| st.dismiss(window, cx));
                        })
                })
                .child(
                    Icon::empty()
                        .path(*icon_path)
                        .small()
                        .text_color(if is_current { primary } else { muted }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .text_size(px(13.))
                                .child(div().font_medium().child(crate::tr!(*label)))
                                .when(is_current, |this| {
                                    this.child(
                                        Icon::new(IconName::Check).xsmall().text_color(primary),
                                    )
                                }),
                        )
                        .child(
                            v_flex()
                                .gap_0p5()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(crate::tr!(*description))
                                .when(is_disabled, |text| text.child(disabled_hint)),
                        ),
                ),
        );
    }

    let mut pane = v_flex().w(px(280.)).child(list);
    if pending_restart {
        pane = pane.child(
            div()
                .px_3()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!("composer.restart_note")),
        );
    }
    pane.with_animation(
        "permission-picker-pop-in",
        Animation::new(Duration::from_millis(150)),
        |element, delta| element.opacity(delta),
    )
    .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_traits_pane(
    spec: &ModelSpec,
    selections: &[agent::OptionSelection],
    ultrathink_armed: bool,
    locked: bool,
    pending_restart: bool,
    store_entity: &Entity<WorkspaceStore>,
    context_window_custom: &Entity<InputState>,
    context_window_custom_error: bool,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let primary = cx.theme().primary;
    let default_suffix = crate::tr!("composer.option_default").into_owned();

    let section_header = |label: &str, cx: &mut Context<PopoverState>| -> AnyElement {
        div()
            .flex_none()
            .px_2()
            .pt_2()
            .pb_1()
            .text_size(px(11.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label.to_string())
            .into_any_element()
    };

    let mut pane = v_flex().w_full().p_1().gap_0p5();

    for descriptor in &spec.options {
        match descriptor {
            OptionDescriptor::Select {
                id,
                label,
                options,
                default_value,
            } => {
                let is_reasoning = id == "reasoningEffort";
                let is_context_window = id == "contextWindow";
                pane = pane.child(section_header(label, cx));
                if is_reasoning && locked {
                    pane = pane.child(
                        div()
                            .flex_none()
                            .px_2()
                            .py_1p5()
                            .text_size(px(13.))
                            .text_color(muted)
                            .child(crate::tr!("composer.ultrathink_locked")),
                    );
                    continue;
                }
                let resolved = (!is_context_window)
                    .then(|| resolved_select_value(id, options, default_value, selections))
                    .flatten();
                let resolved_window = is_context_window
                    .then(|| agent::claude::resolved_context_window(&spec.id, selections));
                for (index, opt) in options.iter().enumerate() {
                    let is_default = default_value.as_deref() == Some(opt.value.as_str());
                    let is_ultra = is_reasoning && opt.value == "ultrathink";
                    let is_selected = if let Some(resolved_window) = resolved_window {
                        agent::claude::parse_context_window_tokens(&serde_json::json!(opt.value))
                            == Some(resolved_window)
                    } else if is_reasoning && ultrathink_armed {
                        is_ultra
                    } else if is_ultra {
                        false
                    } else {
                        resolved.as_deref() == Some(opt.value.as_str())
                    };
                    let mut text = opt.label.clone();
                    if is_default {
                        text.push_str(&default_suffix);
                    }
                    let store = store_entity.clone();
                    let pop = popover.clone();
                    let opt_id = id.clone();
                    let opt_value = opt.value.clone();
                    pane = pane.child(
                        h_flex()
                            .id(gpui::SharedString::from(format!("trait-opt-{id}-{index}")))
                            .flex_none()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .hover(|s| s.bg(cx.theme().muted))
                            .child(div().flex_1().min_w_0().child(text))
                            .when(is_selected, |this| {
                                this.child(Icon::new(IconName::Check).xsmall().text_color(primary))
                            })
                            .on_click(move |_, window, cx| {
                                let opt_id = opt_id.clone();
                                let opt_value = opt_value.clone();
                                store.update(cx, |store, _cx| {
                                    if is_ultra {
                                        store.select_ultrathink();
                                    } else {
                                        store.set_active_option(
                                            opt_id,
                                            Some(serde_json::Value::String(opt_value)),
                                        );
                                    }
                                });
                                pop.update(cx, |st, cx| st.dismiss(window, cx));
                            }),
                    );
                }
                if let Some(resolved_window) = resolved_window {
                    let preset_selected = options.iter().any(|opt| {
                        agent::claude::parse_context_window_tokens(&serde_json::json!(opt.value))
                            == Some(resolved_window)
                    });
                    let custom_selected = !preset_selected;
                    let mut label = crate::tr!("composer.context_window_custom").into_owned();
                    if custom_selected {
                        label.push_str(&format!(
                            " ({})",
                            agent::claude::format_context_window(resolved_window)
                        ));
                    }
                    let input = context_window_custom.clone();
                    pane = pane
                        .child(
                            h_flex()
                                .id("trait-opt-context-window-custom")
                                .flex_none()
                                .w_full()
                                .px_2()
                                .py_1p5()
                                .gap_2()
                                .items_center()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(13.))
                                .hover(|s| s.bg(cx.theme().muted))
                                .child(div().flex_1().min_w_0().child(label))
                                .when(custom_selected, |this| {
                                    this.child(
                                        Icon::new(IconName::Check).xsmall().text_color(primary),
                                    )
                                })
                                .on_click(move |_, window, cx| {
                                    input.update(cx, |state, cx| state.focus(window, cx));
                                }),
                        )
                        .child(
                            v_flex()
                                .px_2()
                                .pb_1()
                                .gap_1()
                                .child(Input::new(context_window_custom).appearance(false))
                                .when(context_window_custom_error, |this| {
                                    this.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(cx.theme().danger)
                                            .child(crate::tr!("composer.context_window_invalid")),
                                    )
                                }),
                        );
                }
            }
            OptionDescriptor::Boolean {
                id,
                label,
                default_value,
            } => {
                pane = pane.child(section_header(label, cx));
                let on = option_selection_bool(selections, id).unwrap_or(*default_value);
                for (index, (value, text)) in [
                    (true, crate::tr!("composer.on").into_owned()),
                    (false, crate::tr!("composer.off").into_owned()),
                ]
                .into_iter()
                .enumerate()
                {
                    let is_selected = on == value;
                    let store = store_entity.clone();
                    let pop = popover.clone();
                    let opt_id = id.clone();
                    pane = pane.child(
                        h_flex()
                            .id(gpui::SharedString::from(format!("trait-opt-{id}-{index}")))
                            .flex_none()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .hover(|s| s.bg(cx.theme().muted))
                            .child(div().flex_1().min_w_0().child(text))
                            .when(is_selected, |this| {
                                this.child(Icon::new(IconName::Check).xsmall().text_color(primary))
                            })
                            .on_click(move |_, window, cx| {
                                let opt_id = opt_id.clone();
                                store.update(cx, |store, _cx| {
                                    store.set_active_option(
                                        opt_id,
                                        Some(serde_json::Value::Bool(value)),
                                    );
                                });
                                pop.update(cx, |st, cx| st.dismiss(window, cx));
                            }),
                    );
                }
            }
        }
    }

    if pending_restart {
        pane = pane.child(
            div()
                .flex_none()
                .px_2()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!("composer.restart_note")),
        );
    }
    div()
        .id("traits-options-scroll")
        .w(px(280.))
        .max_h(px(360.))
        .overflow_y_scroll()
        .child(pane)
        .into_any_element()
}

/// The "⋯" overflow popover: the context chip's usage summary plus the
/// permission / mode chips, shown when the control row collapses at narrow
/// widths.
fn render_overflow_pane(
    usage: Option<TokenUsage>,
    mode: ApprovalMode,
    interaction: InteractionMode,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let item = |icon: Icon, label: String| -> AnyElement {
        h_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_1p5()
            .items_center()
            .rounded(px(6.))
            .text_size(px(13.))
            .text_color(muted)
            .child(icon.small().text_color(muted))
            .child(label)
            .into_any_element()
    };

    let (mode_label, mode_icon) = approval_mode_meta(mode);
    let (interaction_icon, interaction_label) = match interaction {
        InteractionMode::Build => ("icons/box.svg", crate::tr!("composer.build")),
        InteractionMode::Plan => ("icons/ruler.svg", crate::tr!("composer.plan")),
    };
    let next_interaction = match interaction {
        InteractionMode::Build => InteractionMode::Plan,
        InteractionMode::Plan => InteractionMode::Build,
    };
    let interaction_store = store_entity.clone();
    let interaction_popover = popover.clone();
    v_flex()
        .w(px(220.))
        .p_1()
        .gap_0p5()
        .child(item(Icon::new(IconName::Info), context_label(usage)))
        // The permission row stays display-only: its full-width counterpart is
        // an explicit picker, and cycling here would let two stray clicks
        // escalate a Supervised session all the way to Full access.
        .child(item(Icon::empty().path(mode_icon), mode_label))
        .child(
            h_flex()
                .id("overflow-interaction")
                .w_full()
                .px_2()
                .py_1p5()
                .gap_1p5()
                .items_center()
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(13.))
                .text_color(muted)
                .hover(|style| style.bg(cx.theme().muted))
                .child(
                    Icon::empty()
                        .path(interaction_icon)
                        .small()
                        .text_color(muted),
                )
                .child(interaction_label)
                .on_click(move |_, window, cx| {
                    interaction_store.update(cx, |store, _cx| {
                        store.set_interaction_mode(next_interaction)
                    });
                    interaction_popover.update(cx, |state, cx| state.dismiss(window, cx));
                }),
        )
        .into_any_element()
}

/// The circular context-window meter's popover (T3's `ContextWindowMeter`
/// hover card): title, percentage · used/max, a progress bar, and the
/// "<Provider> automatically compacts its context when needed." line.
fn render_context_meter_pane(
    usage: Option<TokenUsage>,
    account_usage: Option<tcode_core::usage::ProviderUsage>,
    provider: Option<ProviderKind>,
    pct: Option<f32>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let overloaded = pct.map(context_meter::is_overloaded).unwrap_or(false);
    let bar_color: Hsla = if overloaded {
        rgb(METER_RED).into()
    } else {
        rgb(METER_BLUE).into()
    };
    let mut pane = v_flex().w(px(256.)).p_3().gap_2();

    // Header: "Context Window" + "N% · used/max" (or just used tokens).
    let used = usage.as_ref().and_then(context_meter::used_tokens);
    let max = usage.and_then(|u| u.context_window);
    let pct_label = context_meter::format_percentage(pct);
    let stat: AnyElement = match (max, pct_label.clone()) {
        (Some(max), Some(pct_label)) => h_flex()
            .gap_1()
            .text_size(px(11.))
            .font_family(cx.theme().mono_font_family.clone())
            .text_color(muted)
            .child(pct_label)
            .child("·")
            .child(format!(
                "{}/{}",
                context_meter::format_tokens(used),
                context_meter::format_tokens(Some(max))
            ))
            .into_any_element(),
        _ => div()
            .text_size(px(11.))
            .font_family(cx.theme().mono_font_family.clone())
            .text_color(muted)
            .child(context_meter::format_tokens(used))
            .into_any_element(),
    };
    pane = pane.child(
        h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(crate::tr!("composer.context_window_title")),
            )
            .child(stat),
    );

    // Progress bar (only when the window size is known).
    if max.is_some() {
        let fraction = pct.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
        pane = pane.child(
            div()
                .w_full()
                .h(px(6.))
                .rounded_full()
                .bg(cx.theme().muted)
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(bar_color)
                        .w(gpui::relative(fraction)),
                ),
        );
    }

    // "Total processed" — the session-cumulative token count, when the provider
    // reports it (a native running total or adapter-side accumulation).
    if let Some(total) = usage.and_then(|u| u.total_processed_tokens) {
        pane = pane.child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .gap_3()
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!("composer.total_processed"))
                .child(context_meter::format_tokens(Some(total))),
        );
    }

    // "<Provider> automatically compacts its context when needed."
    if let Some(provider) = provider {
        pane = pane.child(
            div()
                .pt_1()
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!(
                    "composer.compacts_automatically",
                    provider = provider_label(provider)
                )),
        );
    }

    // Account rate-limit windows, exactly as the provider reported them: a
    // Codex Pro account shows only its weekly window, a Claude Max account
    // shows 5h + weekly + any model-scoped weekly.
    if let Some(account) = account_usage.filter(|a| a.error.is_some() || !a.windows.is_empty()) {
        pane = pane.child(crate::material::faded_hairline(cx));
        // 256px only affords one trailing fact: the plan when the provider
        // named it, otherwise how fresh the numbers are.
        let trailing = account
            .plan
            .as_deref()
            .map(crate::usage::plan_label)
            .unwrap_or_else(|| {
                let ago = crate::time::humanize_ago(
                    crate::time::now_secs().saturating_sub(account.fetched_at),
                );
                crate::tr!("usage.updated", when = ago).into_owned()
            });
        pane = pane.child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .gap_3()
                .text_size(px(11.))
                .font_medium()
                .text_color(muted)
                .child(crate::tr!("usage.title"))
                .child(trailing),
        );
        // The raw provider error is Settings-only; at 256px this pane just
        // says the number is missing.
        if account.error.is_some() {
            pane = pane.child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(crate::tr!("usage.unavailable")),
            );
        } else {
            let now = crate::time::now_secs();
            for window in &account.windows {
                let fill = crate::usage::bar_color(window.used_percent, cx);
                pane = pane.child(
                    v_flex()
                        .w_full()
                        .gap(px(3.))
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .items_center()
                                .gap_2()
                                .text_size(px(11.))
                                .child(
                                    div()
                                        .text_color(muted)
                                        .child(crate::usage::window_label(window)),
                                )
                                .child(
                                    div()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(muted)
                                        .child(crate::usage::percent_label(window.used_percent)),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(4.))
                                .rounded_full()
                                .bg(cx.theme().muted)
                                .child(div().h_full().rounded_full().bg(fill).w(gpui::relative(
                                    window.used_percent.clamp(0.0, 100.0) / 100.0,
                                ))),
                        )
                        .when_some(
                            crate::usage::resets_label(window.resets_at, now),
                            |col, label| {
                                col.child(div().text_size(px(10.5)).text_color(muted).child(label))
                            },
                        ),
                );
            }
        }
    }

    pane.into_any_element()
}
