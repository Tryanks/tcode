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
    pub default_effort: Option<String>,
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
    selected: Option<(ProviderKind, String)>,
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
            selected: Some((provider, model)),
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
        cx: &mut Context<Self>,
    ) {
        let selected = (provider, model.into());
        if self.selected.as_ref() != Some(&selected) {
            self.selected_provider = provider;
            self.selected = Some(selected);
            cx.notify();
        }
    }

    fn options(&self, cx: &App) -> Vec<ModelOption> {
        let state = self.app_state.read(cx);
        let mut options = Vec::new();
        for provider in [ProviderKind::Codex, ProviderKind::ClaudeCode] {
            let catalog = state.models_for(provider);
            for model in state.resolved_models(provider) {
                let default_effort = catalog
                    .iter()
                    .find(|spec| spec.id == model.id)
                    .and_then(default_reasoning_effort);
                options.push(ModelOption {
                    provider,
                    id: model.id,
                    name: model.name,
                    default_effort,
                });
            }
        }
        options
    }

    pub(crate) fn display_name(&self, provider: ProviderKind, model: &str, cx: &App) -> String {
        self.options(cx)
            .into_iter()
            .find(|option| option.provider == provider && option.id == model)
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
                let (provider, model) = self
                    .selected
                    .as_ref()
                    .map(|(provider, model)| (*provider, model.as_str()))
                    .unwrap_or((self.selected_provider, ""));
                let display = self.display_name(provider, model, cx);
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
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(tcode_i18n::tr!("model_picker.no_models")),
                    );
                } else {
                    for (index, option) in available.into_iter().enumerate() {
                        let is_selected = selected.as_ref().is_some_and(|(provider, model)| {
                            *provider == option.provider && model == &option.id
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
                                .rounded(px(6.))
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
                                            picker.selected =
                                                Some((selected.provider, selected.id.clone()));
                                        }
                                        cx.emit(ModelSelected(selected));
                                    });
                                    popover.update(cx, |state, cx| state.dismiss(window, cx));
                                }),
                        );
                    }
                }

                let mut tabs = h_flex()
                    .w_full()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().border);
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
                            .rounded(px(6.))
                            .cursor_pointer()
                            .when(is_selected, |tab| tab.bg(cx.theme().accent).font_medium())
                            .hover(|tab| tab.bg(cx.theme().accent))
                            .child(provider_glyph(provider).xsmall())
                            .child(div().text_size(px(12.)).child(provider_label(provider)))
                            .on_click(move |_, _, cx| {
                                picker.update(cx, |picker, cx| {
                                    picker.selected_provider = provider;
                                    cx.notify();
                                });
                                popover.update(cx, |_, cx| cx.notify());
                            }),
                    );
                }

                v_flex().w(px(390.)).child(tabs).child(
                    div()
                        .w_full()
                        .h(px(300.))
                        .overflow_y_scrollbar()
                        .child(div().size_full().child(rows)),
                )
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
