use super::super::*;
use crate::scroll::ScrollableElement as _;

#[derive(Clone)]
/// One selectable model in the picker (a catalog [`ModelSpec`] row).
struct ModelRow {
    /// Provider-native model id (the favorites key + selection value), or the
    /// ACP agent's registry id when `acp` is set.
    id: String,
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
    /// any favorites exist, else the active session's profile.
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
        let resolved = store.picker_models_for_profile(
            active_profile
                .as_deref()
                .unwrap_or_else(|| tcode_core::settings::Settings::builtin_profile_id(provider)),
        );
        let display = current_model_name_resolved(&resolved, &catalog, current_model.as_deref());

        // Build the filtered row list for the current frame. Favorites open
        // first when any exist. The favorites sweep covers every
        // enabled profile — built-ins *and* third-party (e.g. a Kimi endpoint)
        // — in rail order, so a starred custom-profile model is not lost.
        let query = self.model_search.read(cx).value().to_lowercase();
        let fav_profiles: Vec<(String, ProviderKind)> = {
            let profiles = store.enabled_profiles();
            ProviderKind::NATIVE
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
        let rows: Rc<[ModelRow]> = all_rows
            .into_iter()
            .filter(|r| query.is_empty() || r.name.to_lowercase().contains(&query))
            .collect();
        // Only the built-in profiles have a probed catalog that can still be
        // loading; a third-party profile shows its own slugs immediately.
        let loading = store.models_loading(provider)
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
            .filter(|agent| agent.enabled && agent.offered_for_new_sessions())
            .map(|agent| (agent.id.clone(), agent.name.clone()))
            .collect();

        let trigger = Button::new("model-picker")
            .debug_selector(|| "model-picker".into())
            .when(
                self.compact || composer_state.conversation_read_only,
                |button| button.w_full().max_w(px(160.)).min_w_0().overflow_hidden(),
            )
            .ghost()
            .compact()
            .h(px(28.))
            .when(self.compact, |button| button.min_h(px(44.)).min_w(px(44.)))
            .disabled(!self.interactive(cx))
            .rounded(crate::material::radius_input(cx))
            .child(
                h_flex()
                    .when(
                        self.compact || composer_state.conversation_read_only,
                        |el| el.w_full().min_w_0().overflow_hidden(),
                    )
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .child(tinted_provider_glyph(provider, store).small())
                    .child(
                        div()
                            .when(
                                self.compact || composer_state.conversation_read_only,
                                |el| el.min_w_0().truncate(),
                            )
                            .font_medium()
                            .child(display),
                    )
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );

        // A desktop window types into the search the moment the picker opens.
        // A phone lists models to tap: focusing the search there would raise
        // the software keyboard over the sheet, so the popover keeps its own
        // focus (Escape and Back still close it) and a tap on the search
        // field asks for the keyboard.
        let search_focus = (!crate::window_seam::is_mobile(cx))
            .then(|| self.model_search.read(cx).focus_handle(cx));
        crate::material::overlay_popover(("model-picker-popover", self.model_picker_token), cx)
            .when_some(search_focus, |popover, focus| popover.track_focus(&focus))
            .anchor(Anchor::BottomLeft)
            .when(self.compact, |popover| {
                popover.bottom_sheet(crate::tr!("mobile.model"))
            })
            .default_open(self.model_picker_token > 0 && self.interactive(cx))
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
                    rows,
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
        let descriptors = composer
            .active_option_descriptors
            .iter()
            .filter(|descriptor| {
                matches!(
                    descriptor,
                    OptionDescriptor::Select {
                        role: agent::OptionRole::Model,
                        ..
                    } | OptionDescriptor::Boolean {
                        role: agent::OptionRole::Model,
                        ..
                    }
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        if composer.conversation_read_only {
            let effort =
                option_selection_str(&composer.active_option_selections, "reasoningEffort");
            let descriptor = descriptors.iter().find(|option| {
                matches!(option, OptionDescriptor::Select { id, .. } if id == "reasoningEffort")
            });
            let label = if let Some(OptionDescriptor::Select {
                options,
                default_value,
                ..
            }) = descriptor
            {
                let value = effort.or(default_value.as_deref());
                value.map(|value| {
                    options
                        .iter()
                        .find(|option| option.value == value)
                        .map_or_else(|| value.to_owned(), |option| option.label.clone())
                })
            } else {
                effort.map(str::to_owned)
            }
            .unwrap_or_else(|| crate::tr!("composer.context_unknown").into_owned());
            return Button::new("traits-chip")
                .debug_selector(|| "traits-chip".into())
                .ghost()
                .compact()
                .h(px(28.))
                .when(self.compact, |button| button.min_h(px(44.)).min_w(px(44.)))
                .disabled(true)
                .label(label)
                .into_any_element();
        }
        if descriptors.is_empty() {
            return div().into_any_element();
        }
        let spec = ModelSpec {
            options: descriptors,
            ..composer.active_model_spec.clone().unwrap_or(ModelSpec {
                id: String::new(),
                display_name: String::new(),
                is_default: false,
                options: Vec::new(),
            })
        };
        let selections = composer.active_option_selections;
        // Keep the effort value readable on phones; the sheet still exposes
        // every parameter, including context capacity and service tier.
        let mut chip_spec = spec.clone();
        if self.compact
            && let Some(effort) = spec.options.iter().find(|option| {
                matches!(option, OptionDescriptor::Select { id, .. } if id == "reasoningEffort")
            })
        {
            chip_spec.options = vec![effort.clone()];
        }
        let Some(label) = traits_chip_label(&chip_spec, &selections) else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let pending_restart = composer.options_pending_restart;

        let trigger = Button::new("traits-chip")
            .debug_selector(|| "traits-chip".into())
            .ghost()
            .compact()
            .h(px(28.))
            .when(self.compact, |button| button.min_h(px(44.)).min_w(px(44.)))
            .disabled(!self.interactive(cx))
            .rounded(crate::material::radius_chip(cx))
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(div().whitespace_nowrap().child(label))
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );

        let store_entity = self.workspace_store.clone();
        let composer_entity = cx.entity();
        let context_window_custom = self.context_window_custom.clone();
        crate::material::overlay_popover("traits-popover", cx)
            .anchor(Anchor::BottomLeft)
            .when(self.compact, |popover| {
                popover.bottom_sheet(crate::tr!("mobile.model"))
            })
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
                    composer_entity.read(cx).compact,
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

    /// The circular context-window meter (ring showing used%, red > 90%) and
    /// its hover/click popover.
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
            .debug_selector(|| "context-meter".into())
            .aria_label(crate::tr!("composer.context_window_title").into_owned())
            .ghost()
            .compact()
            .h(px(28.))
            .when(self.compact, |button| button.min_h(px(44.)).min_w(px(44.)))
            .disabled(!self.interactive(cx))
            .rounded(crate::material::radius_chip(cx))
            .child(div().size(px(16.)).child(crate::widgets::ring::ring_canvas(
                pct.unwrap_or(0.0),
                ring_color,
                track,
            )));

        crate::material::overlay_popover("context-popover", cx)
            .anchor(Anchor::BottomLeft)
            .when(self.compact, |popover| {
                popover.bottom_sheet(crate::tr!("composer.context_window_title"))
            })
            .trigger(trigger)
            .content(move |_, window, cx| {
                let compact = crate::window_seam::window_is_compact(window, cx);
                render_context_meter_pane(usage, account_usage.clone(), provider, pct, compact, cx)
            })
            .into_any_element()
    }

    pub(in super::super) fn render_permission_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let composer = self.workspace_store.read(cx).composer_state();
        let Some(control) = PermissionControl::from_composer(&composer) else {
            return permission_notice(&composer, cx);
        };
        let muted = cx.theme().muted_foreground;
        let label = control.shown_label();
        let pending = control.requested.is_some();
        let trigger = Button::new("permission-chip")
            .debug_selector(|| "permission-chip".into())
            .aria_label(crate::tr!("permission.chip", value = label.clone()).into_owned())
            .ghost()
            .compact()
            .h(px(28.))
            .when(self.compact, |button| button.min_h(px(44.)).min_w(px(44.)))
            .disabled(!self.interactive(cx))
            .rounded(crate::material::radius_input(cx))
            .child(
                h_flex()
                    .min_w_0()
                    .overflow_hidden()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(
                        Icon::empty()
                            .path("icons/lock.svg")
                            .small()
                            .text_color(muted),
                    )
                    .child(div().min_w_0().truncate().child(label))
                    .when(pending, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .child(crate::tr!("permission.pending")),
                        )
                    })
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );
        let store_entity = self.workspace_store.clone();
        let compact = self.compact;
        let title = control.label.clone();
        crate::material::overlay_popover("permission-popover", cx)
            .anchor(Anchor::BottomLeft)
            .when(self.compact, |popover| popover.bottom_sheet(title))
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_permission_pane(&control, compact, &store_entity, &cx.entity(), cx)
            })
            .into_any_element()
    }

    /// The phone composer's "+" : a sheet of ways to add to the message.
    /// Only the photo library for now; more rows go here, not in the row.
    pub(in super::super) fn render_attach_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let composer = cx.entity();
        let trigger = Button::new("attach-menu")
            .debug_selector(|| "attach-menu".into())
            .min_w(px(44.))
            .min_h(px(44.))
            .ghost()
            .compact()
            .aria_label(crate::tr!("attach.add").into_owned())
            .child(Icon::new(IconName::Plus).small().text_color(muted));

        crate::material::overlay_popover("attach-popover", cx)
            .anchor(Anchor::BottomLeft)
            .bottom_sheet(crate::tr!("attach.add"))
            .trigger(trigger)
            .content(move |_, _window, cx| {
                let composer = composer.clone();
                let popover = cx.entity();
                let muted = cx.theme().muted_foreground;
                v_flex()
                    .w_full()
                    .p_1()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .id("attach-photo-library")
                            .w_full()
                            .min_h(px(44.))
                            .px_2()
                            .py_1p5()
                            .gap_1p5()
                            .items_center()
                            .rounded(cx.theme().tokens.radius.sm)
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(muted)
                            .hover(|style| style.bg(cx.theme().muted))
                            .child(
                                Icon::empty()
                                    .path("icons/image.svg")
                                    .small()
                                    .text_color(muted),
                            )
                            .child(crate::tr!("attach.photo_library"))
                            .on_click(move |_, window, cx| {
                                popover.update(cx, |state, cx| state.dismiss(window, cx));
                                composer.update(cx, |composer, cx| {
                                    composer.pick_images_from_library(window, cx)
                                });
                            }),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    pub(in super::super) fn render_overflow_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let composer = self.workspace_store.read(cx).composer_state();
        let usage = composer.token_usage;
        let muted = cx.theme().muted_foreground;
        let permission = PermissionControl::from_composer(&composer).map(|control| {
            if control.requested.is_some() {
                format!(
                    "{} · {}",
                    control.shown_label(),
                    crate::tr!("permission.pending")
                )
            } else {
                control.shown_label()
            }
        });
        let trigger = Button::new("overflow-controls")
            .when(self.compact, |button| button.min_w(px(44.)).min_h(px(44.)))
            .ghost()
            .compact()
            .tooltip(crate::tr!("composer.more_controls"))
            .child(Icon::new(IconName::Ellipsis).small().text_color(muted));

        crate::material::overlay_popover("overflow-popover", cx)
            .anchor(Anchor::BottomLeft)
            .when(self.compact, |popover| {
                popover.bottom_sheet(crate::tr!("composer.more_controls"))
            })
            .trigger(trigger)
            .content(move |_, window, cx| {
                render_overflow_pane(usage, permission.clone(), window, cx)
            })
            .into_any_element()
    }
}

#[allow(clippy::too_many_arguments)]
fn render_model_pane(
    rows: Rc<[ModelRow]>,
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
    let compact = composer.read(cx).compact;

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
        crate::material::tab(id, label.clone(), active, cx)
            .flex_none()
            .size(px(if compact { 44. } else { 28. }))
            .rounded(cx.theme().tokens.radius.sm)
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

    let mut rail_col = gpui_base::Tabs::new("model-provider-rail-tabs")
        .flex()
        .flex_col()
        .aria_label(crate::tr!("composer.model_sources"))
        .w_full()
        .py_2()
        .px_1p5()
        .gap_1()
        .child(rail_icon(
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
        ProviderKind::NATIVE
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
        .flex_none()
        .w(px(if compact { 56. } else { 44. }))
        .h_full()
        .border_r_1()
        .border_color(cx.theme().border)
        .overflow_y_scroll_area()
        .child(rail_col);

    let list = div()
        .id("model-picker-list")
        .role(Role::ListBox)
        .aria_label(crate::tr!("composer.model_results"))
        .flex_1()
        .min_h_0();
    let list = if rows.is_empty() {
        list.px_1()
            .py_1()
            .child(
                div()
                    .px_3()
                    .py_4()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(if loading {
                        crate::tr!("composer.loading_models")
                    } else {
                        crate::tr!("composer.no_models")
                    }),
            )
            .into_any_element()
    } else {
        let count = rows.len();
        // Opening the picker shows the current model; a search starts at the
        // top of its results.
        let current = model_search
            .read(cx)
            .value()
            .is_empty()
            .then(|| {
                rows.iter()
                    .position(|row| selected.as_deref() == Some(row.id.as_str()))
            })
            .flatten();
        let rows = rows.clone();
        let selected = selected.clone();
        let store_entity = store_entity.clone();
        let popover = popover.clone();
        list.flex()
            .flex_col()
            .child(
                crate::scroll::VirtualList::measured(
                    "model-picker-rows",
                    count,
                    move |index, _, cx| {
                        div()
                            .when(index + 1 < count, |row| row.pb_0p5())
                            .child(render_model_row(
                                &rows[index],
                                index,
                                &selected,
                                compact,
                                &store_entity,
                                &popover,
                                cx,
                            ))
                    },
                )
                .reveal(current)
                .flex_1()
                .min_h_0()
                .px_1()
                .py_1(),
            )
            .into_any_element()
    };

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
        .child(list);
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

    let key_rows: Vec<ModelRow> = rows.iter().take(9).cloned().collect();
    let store_key = store_entity.clone();
    let popover_key = popover.clone();

    let pane = h_flex()
        .key_context("ModelPicker")
        .when(!composer.read(cx).compact, |pane| pane.w(px(360.)))
        .when(composer.read(cx).compact, |pane| pane.w_full())
        .h(px(360.))
        .items_stretch()
        .rounded(crate::material::radius_card(cx))
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
        .child(pane);
    if compact {
        return v_flex()
            .w_full()
            .min_h_0()
            .child(pane)
            .child(render_compact_model_footer(store_entity, cx))
            .into_any_element();
    }
    pane.with_animation(
        "model-picker-pop-in",
        Animation::new(Duration::from_millis(150)),
        |element, delta| element.opacity(delta),
    )
    .into_any_element()
}

fn render_compact_model_footer(
    store_entity: &Entity<WorkspaceStore>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let composer_state = store_entity.read(cx).composer_state();
    let selections = composer_state.active_option_selections.clone();
    let effort = composer_state
        .active_option_descriptors
        .iter()
        .find_map(|descriptor| match descriptor {
            OptionDescriptor::Select {
                id,
                options,
                default_value,
                ..
            } if id == "reasoningEffort" => Some((
                // Use the localized shared label instead of provider-specific copy.
                crate::tr!("mobile.effort").into_owned(),
                options.clone(),
                resolved_select_value(id, options, default_value, &selections),
            )),
            _ => None,
        });

    let group = |label: gpui::SharedString,
                 track: crate::scroll::ScrollArea<gpui::Stateful<gpui::Div>>,
                 cx: &mut Context<PopoverState>| {
        v_flex()
            .gap(px(6.))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(track)
    };

    let mut footer = v_flex()
        .flex_none()
        .w_full()
        .gap(px(14.))
        .px(px(16.))
        .pt(px(12.));
    if let Some((label, options, current)) = effort {
        let segments = options
            .iter()
            .map(|option| {
                let selected = current.as_deref() == Some(option.value.as_str());
                let store = store_entity.clone();
                let value = option.value.clone();
                crate::material::segment(
                    gpui::SharedString::from(format!("compact-effort-{}", option.value)),
                    option.label.clone(),
                    selected,
                    cx,
                )
                .on_change(move |_, _, _, cx| {
                    let value = value.clone();
                    store.update(cx, |store, _cx| {
                        store.set_active_option(
                            "reasoningEffort".to_string(),
                            Some(serde_json::Value::String(value)),
                        );
                    });
                })
            })
            .collect::<Vec<_>>();
        let track = crate::material::segmented_track("compact-effort", segments, cx);
        footer = footer.child(group(label.into(), track, cx));
    }

    if let Some(control) = PermissionControl::from_composer(&composer_state) {
        let shown = control.requested.as_ref().or(control.current.as_ref());
        let segments = control
            .rows
            .iter()
            .map(|row| {
                let store = store_entity.clone();
                let id = control.id.clone();
                let value = row.value.clone();
                crate::material::segment(
                    gpui::SharedString::from(format!("compact-permission-{}", row.value)),
                    if row.recommended {
                        format!("{} ★", row.label)
                    } else {
                        row.label.clone()
                    },
                    shown == Some(&row.value),
                    cx,
                )
                .disabled(row.unavailable.is_some())
                .on_change(move |_, _, _, cx| {
                    store.update(cx, |store, _cx| {
                        store.set_active_option(id.clone(), Some(value.clone()))
                    });
                })
            })
            .collect::<Vec<_>>();
        let track = crate::material::segmented_track("compact-permission", segments, cx);
        let label = match (control.requested.is_some(), control.apply_hint()) {
            (true, Some(hint)) => {
                format!(
                    "{} · {} · {hint}",
                    control.label,
                    crate::tr!("permission.pending")
                )
            }
            (true, None) => format!("{} · {}", control.label, crate::tr!("permission.pending")),
            (false, _) => control.label.clone(),
        };
        footer = footer.child(group(label.into(), track, cx));
    } else {
        footer = footer.child(permission_notice(&composer_state, cx));
    }
    footer
        .child(div().h(px(8.)).flex_none())
        .id("compact-model-footer")
        .occlude()
        .border_t_1()
        .border_color(cx.theme().border)
        .into_any_element()
}

fn render_model_row(
    row: &ModelRow,
    index: usize,
    selected: &Option<String>,
    compact: bool,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &App,
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
        .min_h(px(if compact { 52. } else { 28. }))
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .rounded(crate::material::radius_chip(cx))
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
        .when(index < 9 && !compact, |this| {
            this.child(
                div()
                    .flex_none()
                    .px_1()
                    .py(px(1.))
                    .rounded(cx.theme().tokens.radius.sm)
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
            .when(compact, |el| {
                el.min_w(px(44.))
                    .min_h(px(44.))
                    .flex()
                    .items_center()
                    .justify_center()
            })
            .rounded(cx.theme().tokens.radius.sm)
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
                    rgb(
                        tcode_core::settings::builtin_provider_color(ProviderKind::ClaudeCode)
                            .expect("Claude has a brand color"),
                    )
                    .into()
                } else {
                    muted
                }),
            )
            .on_click(move |_, window, cx| {
                crate::widgets::stop_click_propagation(window, cx);
                let fav_id = fav_id.clone();
                store_fav.update(cx, |store, _cx| store.toggle_favorite_model(fav_id));
                // Refresh the open popover so the star + ordering update.
                popover_fav.update(cx, |_, cx| cx.notify());
            }),
        )
        .into_any_element()
}

struct PermissionRow {
    value: serde_json::Value,
    label: String,
    description: Option<String>,
    unavailable: Option<String>,
    recommended: bool,
}

struct PermissionControl {
    id: String,
    label: String,
    rows: Vec<PermissionRow>,
    /// The value the provider confirmed.
    current: Option<serde_json::Value>,
    /// A value the user chose that the provider has not confirmed yet.
    requested: Option<serde_json::Value>,
    apply: agent::ApplyTiming,
}

impl PermissionControl {
    fn from_composer(composer: &crate::store::ComposerState) -> Option<Self> {
        let descriptor = composer
            .active_option_descriptors
            .iter()
            .find(|descriptor| {
                matches!(
                    descriptor,
                    OptionDescriptor::Select {
                        role: agent::OptionRole::Permission,
                        ..
                    }
                )
            })
            .cloned()
            .or_else(|| agent::permission_control(composer.provider?))?;
        let (id, label, rows, default, apply) = match descriptor {
            OptionDescriptor::Select {
                id,
                label,
                options,
                default_value,
                apply,
                recommended,
                ..
            } => {
                let rows = options
                    .into_iter()
                    .map(|option| PermissionRow {
                        recommended: recommended.as_deref() == Some(option.value.as_str()),
                        description: option.description.map(|description| {
                            crate::i18n::translate_permission_description(
                                &format!("permission.values.{id}.{}", option.value),
                                &description,
                            )
                        }),
                        value: serde_json::Value::String(option.value),
                        label: crate::i18n::translate_english(
                            "permission.current.label",
                            &option.label,
                        )
                        .into_owned(),
                        unavailable: option.unavailable.map(|reason| {
                            crate::i18n::translate_english("permission.unavailable", &reason)
                                .into_owned()
                        }),
                    })
                    .collect();
                (
                    id,
                    label,
                    rows,
                    default_value.map(serde_json::Value::String),
                    apply,
                )
            }
            OptionDescriptor::Boolean { .. } => return None,
        };
        let current = composer
            .active_option_selections
            .iter()
            .find(|selection| selection.id == id)
            .map(|selection| selection.value.clone())
            .or(default);
        let requested = composer
            .requested_option_selections
            .iter()
            .find(|selection| selection.id == id)
            .map(|selection| selection.value.clone())
            .filter(|value| Some(value) != current.as_ref());
        Some(Self {
            label: crate::i18n::translate_english(&format!("permission.controls.{id}"), &label)
                .into_owned(),
            id,
            rows,
            current,
            requested,
            apply,
        })
    }

    fn row_label(&self, value: Option<&serde_json::Value>) -> Option<String> {
        self.rows
            .iter()
            .find(|row| Some(&row.value) == value)
            .map(|row| row.label.clone())
    }

    /// What the composer shows: the pending choice while one is in flight,
    /// otherwise the provider's confirmed value.
    fn shown_label(&self) -> String {
        self.row_label(self.requested.as_ref().or(self.current.as_ref()))
            .unwrap_or_else(|| self.label.clone())
    }

    fn apply_hint(&self) -> Option<std::borrow::Cow<'static, str>> {
        match self.apply {
            agent::ApplyTiming::Live => None,
            agent::ApplyTiming::NextTurn => Some(crate::tr!("permission.next_turn")),
            agent::ApplyTiming::Restart => Some(crate::tr!("permission.restart")),
        }
    }
}

fn permission_notice(composer: &crate::store::ComposerState, cx: &App) -> AnyElement {
    let Some(notice) = composer.provider.and_then(agent::permission_notice) else {
        return div().into_any_element();
    };
    let notice: gpui::SharedString =
        crate::i18n::translate_english("permission.notice", notice).into();
    let muted = cx.theme().muted_foreground;
    h_flex()
        .id("permission-notice")
        .debug_selector(|| "permission-notice".into())
        .min_w_0()
        .min_h(px(28.))
        .px_2()
        .py_1()
        .gap_1p5()
        .items_center()
        .text_size(px(13.))
        .text_color(muted)
        .child(
            Icon::empty()
                .path("icons/lock.svg")
                .small()
                .text_color(muted),
        )
        .child(notice)
        .into_any_element()
}

fn render_permission_pane(
    control: &PermissionControl,
    compact: bool,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let primary = cx.theme().primary;
    let caption = |text: gpui::SharedString| div().text_size(px(11.)).text_color(muted).child(text);
    let mut list = v_flex()
        .id("permission-menu")
        .role(Role::Menu)
        .aria_label(control.label.clone())
        .w_full()
        .p_1()
        .gap_0p5();
    for (index, option) in control.rows.iter().enumerate() {
        let effective = control.current.as_ref() == Some(&option.value);
        let pending = control.requested.as_ref() == Some(&option.value);
        let disabled = option.unavailable.is_some();
        let store = store_entity.clone();
        let popover = popover.clone();
        let value = option.value.clone();
        let id = control.id.clone();
        list = list.child(
            h_flex()
                .id(("permission-row", index))
                .debug_selector(move || format!("permission-row-{index}"))
                .role(Role::MenuItem)
                .aria_label(
                    std::iter::once(option.label.as_str())
                        .chain(option.description.as_deref())
                        .chain(option.unavailable.as_deref())
                        .collect::<Vec<_>>()
                        .join(": "),
                )
                .aria_selected(effective)
                .when(effective, |row| row.aria_active_descendant())
                .w_full()
                .min_h(px(if compact { 48. } else { 28. }))
                .px_2()
                .py_1()
                .gap_2()
                .items_start()
                .rounded(crate::material::radius_chip(cx))
                .when(effective, |row| row.bg(cx.theme().list_active))
                .when(disabled, |row| row.opacity(0.55))
                .when(!disabled, |row| {
                    row.cursor_pointer()
                        .hover(|style| style.bg(cx.theme().muted))
                        .on_click(move |_, window, cx| {
                            store.update(cx, |store, _cx| {
                                store.set_active_option(id.clone(), Some(value.clone()))
                            });
                            popover.update(cx, |state, cx| state.dismiss(window, cx));
                        })
                })
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .flex_wrap()
                                .text_size(px(13.))
                                .child(div().font_medium().child(option.label.clone()))
                                .when(option.recommended, |row| {
                                    row.child(caption(crate::tr!("permission.recommended").into()))
                                })
                                .when(effective, |row| {
                                    row.child(
                                        Icon::new(IconName::Check).xsmall().text_color(primary),
                                    )
                                })
                                .when(pending, |row| {
                                    row.child(caption(crate::tr!("permission.pending").into()))
                                }),
                        )
                        .when_some(option.description.clone(), |column, description| {
                            column.child(caption(description.into()))
                        })
                        .when_some(option.unavailable.clone(), |column, reason| {
                            column.child(caption(reason.into()))
                        }),
                ),
        );
    }
    let mut pane = v_flex()
        .debug_selector(|| "permission-pane".into())
        .w_full()
        .when(!compact, |pane| pane.w(px(280.)))
        .child(list);
    if let Some(hint) = control.apply_hint() {
        pane = pane.child(
            div()
                .px_3()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(caption(hint.into())),
        );
    }
    if compact {
        return pane.into_any_element();
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
    compact: bool,
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

    // The bolt in the corner owns fast mode: the Claude `fastMode` rows are
    // dropped and a Codex service-tier list keeps only its non-fast tiers
    // (hidden entirely when Standard is all that is left).
    let fast = FastMode::of(spec);
    for descriptor in &spec.options {
        let fast_owned = fast.as_ref().is_some_and(|fast| {
            let (OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. }) =
                descriptor;
            *id == fast.option_id
        });
        match descriptor {
            OptionDescriptor::Select {
                id,
                label,
                options,
                default_value,
                ..
            } => {
                let options: Vec<agent::SelectOption> = if fast_owned {
                    options
                        .iter()
                        .filter(|choice| !FastMode::is_fast_tier(choice))
                        .cloned()
                        .collect()
                } else {
                    options.clone()
                };
                if fast_owned && options.len() < 2 {
                    continue;
                }
                let options = &options;
                pane = pane.child(section_header(label, cx));
                let resolved = resolved_select_value(id, options, default_value, selections);
                let resolved_window = (id == "contextWindow")
                    .then(|| {
                        selections
                            .iter()
                            .find(|selection| selection.id == *id)
                            .and_then(|selection| {
                                agent::claude::parse_context_window_tokens(&selection.value)
                            })
                            .or_else(|| {
                                default_value.as_ref().and_then(|value| {
                                    agent::claude::parse_context_window_tokens(&serde_json::json!(
                                        value
                                    ))
                                })
                            })
                    })
                    .flatten();
                for (index, opt) in options.iter().enumerate() {
                    let is_default = default_value.as_deref() == Some(opt.value.as_str());
                    let is_selected = if let Some(window) = resolved_window {
                        agent::claude::parse_context_window_tokens(&serde_json::json!(opt.value))
                            == Some(window)
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
                            .when(compact, |row| row.min_h(px(44.)))
                            .flex_none()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .rounded(cx.theme().tokens.radius.sm)
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
                                    store.set_active_option(
                                        opt_id,
                                        Some(serde_json::Value::String(opt_value)),
                                    );
                                });
                                pop.update(cx, |st, cx| st.dismiss(window, cx));
                            }),
                    );
                }
                if id == "contextWindow" {
                    let custom_selected = resolved_window.is_some_and(|window| {
                        !options.iter().any(|opt| {
                            agent::claude::parse_context_window_tokens(&serde_json::json!(
                                opt.value
                            )) == Some(window)
                        })
                    });
                    let mut label = crate::tr!("composer.context_window_custom").into_owned();
                    if custom_selected {
                        label.push_str(&format!(
                            " ({})",
                            agent::claude::format_context_window(resolved_window.unwrap())
                        ));
                    }
                    let input = context_window_custom.clone();
                    pane = pane
                        .child(
                            h_flex()
                                .id("trait-opt-context-window-custom")
                                .when(compact, |row| row.min_h(px(44.)))
                                .flex_none()
                                .w_full()
                                .px_2()
                                .py_1p5()
                                .gap_2()
                                .items_center()
                                .rounded(cx.theme().tokens.radius.sm)
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
                ..
            } => {
                if fast_owned {
                    continue;
                }
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
                            .when(compact, |row| row.min_h(px(44.)))
                            .flex_none()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .rounded(cx.theme().tokens.radius.sm)
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
        .relative()
        .w_full()
        .when(!compact, |pane| pane.w(px(280.)))
        .child(
            div()
                .id("traits-options-scroll")
                .w_full()
                .max_h(px(360.))
                .overflow_y_scroll_area()
                .child(pane),
        )
        .child(render_fast_mode_bolt(
            fast.as_ref(),
            selections,
            compact,
            store_entity,
            cx,
        ))
        .into_any_element()
}

/// The fast-mode bolt pinned to the traits pane's top-right corner: filled
/// amber when on, an outline when off, dimmed and inert when the model has no
/// fast mode. Toggling keeps the pane open so the new state is visible. The
/// wrapper occludes the list scrolling beneath it so hovering or clicking the
/// bolt never reaches an option row under it.
fn render_fast_mode_bolt(
    fast: Option<&FastMode>,
    selections: &[agent::OptionSelection],
    compact: bool,
    store_entity: &Entity<WorkspaceStore>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let on = fast.map(|fast| fast.enabled(selections));
    let (path, color) = match on {
        Some(true) => ("icons/zap-filled.svg", theme.fast_mode_accent()),
        _ => ("icons/zap.svg", theme.muted_foreground),
    };
    let icon = Icon::empty().path(path).text_color(color);
    let wrapper = div()
        .absolute()
        .top_1()
        .right_1()
        .block_mouse_except_scroll();
    let Some(fast) = fast else {
        // Inert: no hover affordance, just the desktop tooltip explaining why.
        let tooltip = crate::tr!("composer.fast_mode_unsupported");
        return wrapper
            .child(
                div()
                    .id("traits-fast-mode")
                    .size(px(if compact {
                        crate::material::TOUCH_TARGET
                    } else {
                        24.
                    }))
                    .flex()
                    .items_center()
                    .justify_center()
                    .opacity(0.5)
                    .cursor_default()
                    .when(!compact, |el| {
                        el.tooltip(move |window, cx| {
                            crate::widgets::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
                        })
                    })
                    .child(if compact {
                        icon.size(px(20.))
                    } else {
                        icon.small()
                    }),
            )
            .into_any_element();
    };
    let fast = fast.clone();
    let store = store_entity.clone();
    wrapper
        .child(
            crate::material::toolbar_icon_button(
                "traits-fast-mode",
                icon,
                crate::tr!("composer.fast_mode"),
                compact,
            )
            .debug_selector(|| "traits-fast-mode".into())
            .on_click(move |_, window, cx| {
                crate::widgets::stop_click_propagation(window, cx);
                let value = fast.value(on != Some(true));
                store.update(cx, |store, _cx| {
                    store.set_active_option(fast.option_id.clone(), Some(value));
                });
            }),
        )
        .into_any_element()
}

fn render_overflow_pane(
    usage: Option<TokenUsage>,
    permission: Option<String>,
    window: &Window,
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
            .rounded(cx.theme().tokens.radius.sm)
            .text_size(px(13.))
            .text_color(muted)
            .child(icon.small().text_color(muted))
            .child(label)
            .into_any_element()
    };

    v_flex()
        .w_full()
        .when(!crate::window_seam::window_is_compact(window, cx), |pane| {
            pane.w(px(220.))
        })
        .p_1()
        .gap_0p5()
        .child(item(Icon::new(IconName::Info), context_label(usage)))
        .when_some(permission, |pane, label| {
            pane.child(item(Icon::empty().path("icons/lock.svg"), label))
        })
        .into_any_element()
}

/// Context usage, model capacity and provider rate-limit windows.
fn render_context_meter_pane(
    usage: Option<TokenUsage>,
    account_usage: Option<tcode_core::usage::ProviderUsage>,
    provider: Option<ProviderKind>,
    pct: Option<f32>,
    compact: bool,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let overloaded = pct.map(context_meter::is_overloaded).unwrap_or(false);
    let bar_color: Hsla = if overloaded {
        rgb(METER_RED).into()
    } else {
        rgb(METER_BLUE).into()
    };
    let mut pane = v_flex()
        .debug_selector(|| "context-pane".into())
        .w_full()
        .when(!compact, |pane| pane.w(px(256.)).p_3())
        .gap_2();

    let used = usage.as_ref().and_then(context_meter::used_tokens);
    let max = usage.and_then(|u| u.context_window);
    let pct_label = context_meter::format_percentage(pct);
    let stat: AnyElement = match max {
        Some(max) => h_flex()
            .gap_1()
            .text_size(px(11.))
            .font_family(cx.theme().mono_font_family.clone())
            .text_color(muted)
            .when_some(pct_label, |row, label| row.child(label).child("·"))
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

    if let Some(pct) = pct {
        let fraction = pct.clamp(0.0, 100.0) / 100.0;
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

    let freshness = match usage.map(|u| u.freshness) {
        Some(agent::ContextFreshness::Current) if used.is_some() => "composer.context_latest",
        Some(agent::ContextFreshness::LastKnown) => "composer.context_last_known",
        Some(agent::ContextFreshness::Compacting) => "chat.context_compacting",
        _ => "composer.context_updating",
    };
    pane = pane.child(
        div()
            .text_size(px(11.))
            .text_color(muted)
            .child(crate::tr!(freshness)),
    );

    // "Total processed" — the session-cumulative token count, when the provider
    // reports it (a native running total or timeline accumulation).
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

    // Show only the rate-limit windows reported by the provider.
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

#[cfg(test)]
mod sheet_tests {
    use super::*;
    use gpui::{Render, TestAppContext, size};

    struct PickerHarness {
        store: Entity<WorkspaceStore>,
        context: bool,
    }

    impl Render for PickerHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let compact = crate::window_seam::window_is_compact(window, cx);
            let store = self.store.clone();
            let context = self.context;
            div().size_full().p_4().child(
                crate::material::overlay_popover("picker-regression", cx)
                    .when(compact, |popover| popover.bottom_sheet("Details"))
                    .trigger(
                        Button::new("open")
                            .label("Open")
                            .debug_selector(|| "picker-open".into()),
                    )
                    .content(move |_, _, cx| {
                        if context {
                            render_context_meter_pane(None, None, None, None, compact, cx)
                        } else {
                            let mut composer = store.read(cx).composer_state();
                            composer.provider = Some(ProviderKind::ClaudeCode);
                            let control = PermissionControl::from_composer(&composer).unwrap();
                            render_permission_pane(&control, compact, &store, &cx.entity(), cx)
                        }
                    }),
            )
        }
    }

    /// A phone with a status bar and a keyboard gets a bottom sheet; a tablet
    /// in landscape keeps the desktop-width popover.
    #[gpui::test]
    fn picker_sheets_span_window_and_preserve_wide_width(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(std::env::temp_dir().join(format!(
                "tcode-sheet-test-{}",
                tcode_services::store::now_millis()
            )))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        for context in [false, true] {
            for (width, bottom) in [(393., 34.), (393., 300.), (1024., 0.)] {
                let (_, cx) = cx.add_window_view(|_, _| PickerHarness {
                    store: store.clone(),
                    context,
                });
                cx.simulate_resize(size(px(width), px(852.)));
                crate::window_seam::occlude_for_test(
                    cx,
                    gpui::Edges {
                        top: px(47.),
                        bottom: px(bottom),
                        ..Default::default()
                    },
                );
                cx.update(|window, cx| window.draw(cx).clear(cx));
                let trigger = cx.debug_bounds("picker-open").unwrap().center();
                cx.simulate_click(trigger, Default::default());
                cx.update(|window, cx| window.draw(cx).clear(cx));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                let pane = cx
                    .debug_bounds(if context {
                        "context-pane"
                    } else {
                        "permission-pane"
                    })
                    .unwrap();
                if width < 900. {
                    let sheet = cx.debug_bounds("touch-picker-sheet").unwrap();
                    assert_eq!(sheet.left(), px(0.));
                    assert_eq!(sheet.right(), px(393.));
                    assert_eq!(sheet.bottom(), px(852. - bottom));
                    assert!(sheet.top() >= px(99.));
                    assert_eq!(pane.left(), px(16.));
                    assert_eq!(pane.right(), px(377.));
                    let scrim = cx.debug_bounds("touch-picker-backdrop").unwrap();
                    assert_eq!(scrim.origin, gpui::point(px(0.), px(0.)));
                    assert_eq!(scrim.size, size(px(393.), px(852.)));
                } else {
                    assert_eq!(pane.size.width, px(if context { 256. } else { 280. }));
                }
            }
        }
    }
}
