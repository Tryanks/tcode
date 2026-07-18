//! Reusable native-provider model picker used by settings surfaces.
//!
//! The profile tabs (one per provider profile, built-in or user-created),
//! resolved model catalog, offline/custom-model handling, and selection event
//! live here so Orchestrate and other settings do not grow subtly different
//! pickers.

use gpui::{
    App, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, button::Button, h_flex,
    popover::Popover, scroll::ScrollableElement as _, v_flex,
};

use agent::{OptionDescriptor, ProviderKind};
use tcode_core::settings::Settings;
use tcode_runtime::app::AppState;

use crate::provider_card::provider_glyph;
use crate::settings::provider_label;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelOption {
    pub provider: ProviderKind,
    pub id: String,
    pub name: String,
    pub effort: Option<String>,
    pub profile_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelSelected(pub ModelOption);

#[derive(Clone)]
enum TriggerKind {
    Add(SharedString),
    Selection,
}

pub(crate) struct ProviderModelPicker {
    app_state: Entity<AppState>,
    popover_id: &'static str,
    trigger_id: &'static str,
    trigger_kind: TriggerKind,
    /// The active tab: a profile id (built-in or user-created).
    selected_profile: String,
    selected: Option<(ProviderKind, String, Option<String>)>,
    excluded: Vec<(ProviderKind, String)>,
    _app_subscription: Subscription,
}

impl EventEmitter<ModelSelected> for ProviderModelPicker {}

impl ProviderModelPicker {
    pub fn add(
        app_state: Entity<AppState>,
        popover_id: &'static str,
        trigger_id: &'static str,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let app_subscription = cx.observe(&app_state, |_, _, cx| cx.notify());
        Self {
            app_state,
            popover_id,
            trigger_id,
            trigger_kind: TriggerKind::Add(label.into()),
            selected_profile: Settings::builtin_profile_id(ProviderKind::Codex).to_string(),
            selected: None,
            excluded: Vec::new(),
            _app_subscription: app_subscription,
        }
    }

    pub fn selection(
        app_state: Entity<AppState>,
        popover_id: &'static str,
        trigger_id: &'static str,
        provider: ProviderKind,
        model: impl Into<String>,
        profile_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let app_subscription = cx.observe(&app_state, |_, _, cx| cx.notify());
        let model = model.into();
        Self {
            app_state,
            popover_id,
            trigger_id,
            trigger_kind: TriggerKind::Selection,
            selected_profile: selection_profile_id(provider, profile_id.as_deref()),
            selected: Some((provider, model, profile_id)),
            excluded: Vec::new(),
            _app_subscription: app_subscription,
        }
    }

    pub fn set_excluded(&mut self, excluded: Vec<(ProviderKind, String)>, cx: &mut Context<Self>) {
        if self.excluded != excluded {
            self.excluded = excluded;
            cx.notify();
        }
    }

    pub fn set_selected(
        &mut self,
        provider: ProviderKind,
        model: impl Into<String>,
        profile_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let selected = (provider, model.into(), profile_id);
        if self.selected.as_ref() != Some(&selected) {
            self.selected_profile = selection_profile_id(provider, selected.2.as_deref());
            self.selected = Some(selected);
            cx.notify();
        }
    }

    fn options(&self, cx: &App) -> Vec<ModelOption> {
        let state = self.app_state.read(cx);
        let mut options = Vec::new();
        for profile in state.all_profiles() {
            let catalog = state.models_for(profile.kind);
            let profile_id =
                (!Settings::is_builtin_profile_id(&profile.id)).then_some(profile.id.clone());
            for model in state.picker_models_for_profile(&profile.id) {
                let effort = catalog
                    .iter()
                    .find(|spec| spec.id == model.id)
                    .and_then(default_reasoning_effort);
                options.push(ModelOption {
                    provider: profile.kind,
                    id: model.id,
                    name: model.name,
                    effort,
                    profile_id: profile_id.clone(),
                });
            }
        }
        options
    }

    pub(crate) fn display_name(
        &self,
        provider: ProviderKind,
        model: &str,
        profile_id: Option<&str>,
        cx: &App,
    ) -> String {
        self.options(cx)
            .into_iter()
            .find(|option| {
                option.provider == provider
                    && option.id == model
                    && option.profile_id.as_deref() == profile_id
            })
            .map(|option| option.name)
            .unwrap_or_else(|| model.to_string())
    }

    fn trigger(&self, cx: &Context<Self>) -> Button {
        match &self.trigger_kind {
            TriggerKind::Add(label) => Button::new(self.trigger_id)
                .outline()
                .small()
                .icon(IconName::Plus)
                .label(label.clone()),
            TriggerKind::Selection => {
                let (provider, model, profile_id) = self
                    .selected
                    .as_ref()
                    .map(|(provider, model, profile_id)| {
                        (*provider, model.as_str(), profile_id.as_deref())
                    })
                    .unwrap_or_else(|| {
                        let kind = self.app_state.read(cx).profile_kind(&self.selected_profile);
                        (kind, "", None)
                    });
                let display = self.display_name(provider, model, profile_id, cx);
                Button::new(self.trigger_id).outline().compact().child(
                    h_flex()
                        .w(px(230.))
                        .items_center()
                        .gap_2()
                        .text_size(px(13.))
                        .child(provider_glyph(provider).small())
                        .child(div().flex_1().min_w_0().child(display))
                        .child(
                            Icon::new(IconName::ChevronDown)
                                .xsmall()
                                .text_color(cx.theme().muted_foreground),
                        ),
                )
            }
        }
    }
}

impl Render for ProviderModelPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picker = cx.entity();
        Popover::new(self.popover_id)
            // T3 overlay contour: one panel surface (popover fill + hairline +
            // shadow_xl at the 14px overlay radius). The pane content is transparent
            // so the popup reads as a single card, matching the composer picker.
            .rounded(crate::material::radius_overlay())
            .shadow_xl()
            .trigger(self.trigger(cx))
            .content(move |_, _, cx| {
                let (options, profiles, selected_profile, selected, excluded) = {
                    let picker = picker.read(cx);
                    (
                        picker.options(cx),
                        picker.app_state.read(cx).all_profiles(),
                        picker.selected_profile.clone(),
                        picker.selected.clone(),
                        picker.excluded.clone(),
                    )
                };
                // A deleted profile falls back to the first tab (built-ins
                // always exist, so the list is never empty).
                let current_profile = if profiles.iter().any(|p| p.id == selected_profile) {
                    selected_profile
                } else {
                    profiles.first().map(|p| p.id.clone()).unwrap_or_default()
                };
                let available: Vec<_> = options
                    .into_iter()
                    .filter(|option| {
                        option_profile_id(option) == current_profile
                            && !excluded.iter().any(|(provider, model)| {
                                *provider == option.provider && model == &option.id
                            })
                    })
                    .collect();

                let mut rows = v_flex().w_full().p_1().gap_0p5();
                if available.is_empty() {
                    rows = rows.child(
                        div()
                            .flex_none()
                            .p_4()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(tcode_i18n::tr!("model_picker.no_models")),
                    );
                } else {
                    for (index, option) in available.into_iter().enumerate() {
                        let is_selected =
                            selected
                                .as_ref()
                                .is_some_and(|(provider, model, profile_id)| {
                                    *provider == option.provider
                                        && model == &option.id
                                        && profile_id == &option.profile_id
                                });
                        let picker = picker.clone();
                        let popover = cx.entity();
                        rows = rows.child(
                            h_flex()
                                .id(("settings-model-option", index))
                                .flex_none()
                                .w_full()
                                .px_2()
                                .py_1p5()
                                .gap_2()
                                .items_center()
                                .rounded(crate::material::radius_button())
                                .cursor_pointer()
                                .hover(|style| style.bg(cx.theme().accent))
                                .child(provider_glyph(option.provider).small())
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(div().text_size(px(13.)).child(option.name.clone()))
                                        .child(
                                            div()
                                                .font_family("monospace")
                                                .text_size(px(11.))
                                                .text_color(cx.theme().muted_foreground)
                                                .child(option.id.clone()),
                                        ),
                                )
                                .when(is_selected, |row| {
                                    row.child(Icon::new(IconName::Check).xsmall())
                                })
                                .on_click(move |_, window, cx| {
                                    let selected = option.clone();
                                    picker.update(cx, |picker, cx| {
                                        picker.selected_profile = option_profile_id(&selected);
                                        if matches!(picker.trigger_kind, TriggerKind::Selection) {
                                            picker.selected = Some((
                                                selected.provider,
                                                selected.id.clone(),
                                                selected.profile_id.clone(),
                                            ));
                                        }
                                        cx.emit(ModelSelected(selected));
                                    });
                                    popover.update(cx, |state, cx| state.dismiss(window, cx));
                                }),
                        );
                    }
                }

                // One tab per profile: the two built-ins first, then the user's
                // profiles in their card order.
                let mut tabs = h_flex().w_full().p_1().gap_1();
                for (tab_index, profile) in profiles.iter().enumerate() {
                    let is_selected = profile.id == current_profile;
                    let label = if Settings::is_builtin_profile_id(&profile.id) {
                        provider_label(profile.kind).to_string()
                    } else {
                        profile
                            .settings
                            .display_name
                            .as_deref()
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .unwrap_or(&profile.id)
                            .to_string()
                    };
                    let kind = profile.kind;
                    let profile_id = profile.id.clone();
                    let picker = picker.clone();
                    let popover = cx.entity();
                    tabs = tabs.child(
                        h_flex()
                            .id(("settings-provider-tab", tab_index))
                            .flex_1()
                            .h(px(30.))
                            .items_center()
                            .justify_center()
                            .gap_1p5()
                            .rounded(crate::material::radius_button())
                            .cursor_pointer()
                            .when(is_selected, |tab| tab.bg(cx.theme().accent).font_medium())
                            .hover(|tab| tab.bg(cx.theme().accent))
                            .child(provider_glyph(kind).xsmall())
                            .child(div().text_size(px(13.)).child(label))
                            .on_click(move |_, _, cx| {
                                picker.update(cx, |picker, cx| {
                                    picker.selected_profile = profile_id.clone();
                                    cx.notify();
                                });
                                popover.update(cx, |_, cx| cx.notify());
                            }),
                    );
                }

                v_flex()
                    .w(px(390.))
                    .child(tabs)
                    .child(crate::material::faded_hairline(cx))
                    .child(
                        div()
                            .w_full()
                            .h(px(300.))
                            .overflow_y_scrollbar()
                            .child(div().size_full().child(rows)),
                    )
            })
    }
}

/// The profile a selection belongs to: its explicit profile id, or the kind's
/// built-in profile.
fn selection_profile_id(provider: ProviderKind, profile_id: Option<&str>) -> String {
    profile_id
        .map(str::to_string)
        .unwrap_or_else(|| Settings::builtin_profile_id(provider).to_string())
}

/// The profile tab an option files under (see [`selection_profile_id`]).
fn option_profile_id(option: &ModelOption) -> String {
    selection_profile_id(option.provider, option.profile_id.as_deref())
}

fn default_reasoning_effort(spec: &agent::ModelSpec) -> Option<String> {
    spec.options.iter().find_map(|option| match option {
        OptionDescriptor::Select {
            id, default_value, ..
        } if id == "reasoningEffort" => default_value.clone(),
        _ => None,
    })
}
