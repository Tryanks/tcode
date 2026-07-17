//! Reusable native-provider model picker used by settings surfaces.
//!
//! The provider tabs, resolved model catalog, offline/custom-model handling,
//! and selection event live here so Orchestrate and other settings do not grow
//! subtly different pickers.

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
    selected_provider: ProviderKind,
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
            selected_provider: ProviderKind::Codex,
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
            selected_provider: provider,
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
            self.selected_provider = provider;
            self.selected = Some(selected);
            cx.notify();
        }
    }

    fn options(&self, cx: &App) -> Vec<ModelOption> {
        let state = self.app_state.read(cx);
        let mut options = Vec::new();
        for profile in state.all_profiles() {
            let catalog = state.models_for(profile.kind);
            let profile_id = (!tcode_core::settings::Settings::is_builtin_profile_id(&profile.id))
                .then_some(profile.id.clone());
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
                    .unwrap_or((self.selected_provider, "", None));
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
            .trigger(self.trigger(cx))
            .content(move |_, _, cx| {
                let (options, selected_provider, selected, excluded) = {
                    let picker = picker.read(cx);
                    (
                        picker.options(cx),
                        picker.selected_provider,
                        picker.selected.clone(),
                        picker.excluded.clone(),
                    )
                };
                let available: Vec<_> = options
                    .into_iter()
                    .filter(|option| {
                        option.provider == selected_provider
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
                    let show_headers = available.iter().any(|option| option.profile_id.is_some());
                    let mut previous_profile_id: Option<Option<String>> = None;
                    for (index, option) in available.into_iter().enumerate() {
                        if show_headers && previous_profile_id.as_ref() != Some(&option.profile_id)
                        {
                            let label = option.profile_id.as_deref().map_or_else(
                                || provider_label(option.provider).to_string(),
                                |id| {
                                    let settings =
                                        picker.read(cx).app_state.read(cx).profile_settings(id);
                                    settings
                                        .display_name
                                        .as_deref()
                                        .map(str::trim)
                                        .filter(|name| !name.is_empty())
                                        .unwrap_or(id)
                                        .to_string()
                                },
                            );
                            rows = rows.child(
                                div()
                                    .flex_none()
                                    .px_2()
                                    .pt_2()
                                    .pb_1()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(label),
                            );
                            previous_profile_id = Some(option.profile_id.clone());
                        }
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
                                        picker.selected_provider = selected.provider;
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

                let mut tabs = h_flex().w_full().p_1().gap_1();
                for (tab_index, provider) in [ProviderKind::Codex, ProviderKind::ClaudeCode]
                    .into_iter()
                    .enumerate()
                {
                    let is_selected = provider == selected_provider;
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
                            .child(provider_glyph(provider).xsmall())
                            .child(div().text_size(px(13.)).child(provider_label(provider)))
                            .on_click(move |_, _, cx| {
                                picker.update(cx, |picker, cx| {
                                    picker.selected_provider = provider;
                                    cx.notify();
                                });
                                popover.update(cx, |_, cx| cx.notify());
                            }),
                    );
                }

                crate::material::overlay_contour(
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
                        ),
                    cx,
                )
                .rounded(crate::material::radius_overlay())
            })
    }
}

fn default_reasoning_effort(spec: &agent::ModelSpec) -> Option<String> {
    spec.options.iter().find_map(|option| match option {
        OptionDescriptor::Select {
            id, default_value, ..
        } if id == "reasoningEffort" => default_value.clone(),
        _ => None,
    })
}
