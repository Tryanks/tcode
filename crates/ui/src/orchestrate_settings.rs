//! Settings → Orchestrate: lead-model identities and the child-model routing
//! allow list. Every main model is eligible; only child dispatch is gated.

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    switch::Switch,
    v_flex,
};

use agent::ProviderKind;
use tcode_runtime::app::AppState;

use crate::provider_card::provider_glyph;
use crate::provider_model_picker::{ModelOption, ProviderModelPicker};
use crate::settings::{
    OrchestrateChildModel, OrchestrateSettings, OrchestratorIdentity, Settings, provider_label,
};

struct IdentityRowState {
    provider: ProviderKind,
    model: String,
    identity: Entity<InputState>,
}

struct ChildRowState {
    provider: ProviderKind,
    model: String,
    description: Entity<InputState>,
}

pub struct OrchestrateSettingsPanel {
    app_state: Entity<AppState>,
    generic_identity: Entity<InputState>,
    identity_rows: Vec<IdentityRowState>,
    child_rows: Vec<ChildRowState>,
    identity_model_picker: Entity<ProviderModelPicker>,
    child_model_picker: Entity<ProviderModelPicker>,
    _subscriptions: Vec<Subscription>,
    input_subscriptions: Vec<Subscription>,
}

impl OrchestrateSettingsPanel {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let generic_value = app_state
            .read(cx)
            .settings
            .orchestrate
            .generic_identity
            .clone();
        let generic_identity = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(4, 14)
                .placeholder(tcode_i18n::tr!("orchestrate.generic_identity.placeholder"))
                .default_value(generic_value)
        });
        let identity_model_picker = cx.new(|cx| {
            ProviderModelPicker::add(
                app_state.clone(),
                "orchestrate-add-identity-popover",
                "orchestrate-add-identity",
                tcode_i18n::tr!("orchestrate.model_identity.add"),
                cx,
            )
        });
        let child_model_picker = cx.new(|cx| {
            ProviderModelPicker::add(
                app_state.clone(),
                "orchestrate-add-child-popover",
                "orchestrate-add-child",
                tcode_i18n::tr!("orchestrate.children.add"),
                cx,
            )
        });
        let subscriptions = vec![
            cx.observe(&app_state, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &identity_model_picker,
                window,
                |this, _, event, window, cx| {
                    this.add_identity(&event.0, window, cx);
                },
            ),
            cx.subscribe_in(&child_model_picker, window, |this, _, event, window, cx| {
                this.add_child(&event.0, window, cx);
            }),
        ];
        let mut panel = Self {
            app_state,
            generic_identity,
            identity_rows: Vec::new(),
            child_rows: Vec::new(),
            identity_model_picker,
            child_model_picker,
            _subscriptions: subscriptions,
            input_subscriptions: Vec::new(),
        };
        let generic = panel.generic_identity.clone();
        panel
            .input_subscriptions
            .push(cx.subscribe(&generic, |this, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.commit_generic_identity(cx);
                }
            }));
        panel.rebuild_rows(window, cx);
        panel
    }

    fn update_settings(&self, mutate: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, cx| {
            let mut settings = state.settings.clone();
            mutate(&mut settings);
            state.update_settings(settings, cx);
        });
    }

    fn commit_generic_identity(&self, cx: &mut Context<Self>) {
        let identity = self.generic_identity.read(cx).value().to_string();
        self.update_settings(
            move |settings| settings.orchestrate.generic_identity = identity,
            cx,
        );
    }

    fn rebuild_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Preserve the generic input subscription at index zero.
        self.input_subscriptions.truncate(1);
        self.identity_rows.clear();
        self.child_rows.clear();
        let orchestrate = self.app_state.read(cx).settings.orchestrate.clone();

        for entry in orchestrate.model_identities {
            let identity = cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .auto_grow(3, 10)
                    .placeholder(tcode_i18n::tr!("orchestrate.model_identity.placeholder"))
                    .default_value(entry.identity)
            });
            let provider = entry.provider;
            let model = entry.model.clone();
            self.input_subscriptions
                .push(cx.subscribe(&identity, move |this, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.commit_model_identity(provider, &model, cx);
                    }
                }));
            self.identity_rows.push(IdentityRowState {
                provider: entry.provider,
                model: entry.model,
                identity,
            });
        }

        for entry in orchestrate.child_models {
            let description = cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .auto_grow(3, 9)
                    .placeholder(tcode_i18n::tr!(
                        "orchestrate.children.description_placeholder"
                    ))
                    .default_value(entry.description)
            });
            let provider = entry.provider;
            let model = entry.model.clone();
            self.input_subscriptions
                .push(cx.subscribe(&description, move |this, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.commit_child_definition(provider, &model, cx);
                    }
                }));
            self.child_rows.push(ChildRowState {
                provider: entry.provider,
                model: entry.model,
                description,
            });
        }
        self.sync_picker_exclusions(cx);
    }

    fn sync_picker_exclusions(&self, cx: &mut Context<Self>) {
        let identities = self
            .identity_rows
            .iter()
            .map(|row| (row.provider, row.model.clone()))
            .collect();
        self.identity_model_picker
            .update(cx, |picker, cx| picker.set_excluded(identities, cx));

        let children = self
            .child_rows
            .iter()
            .map(|row| (row.provider, row.model.clone()))
            .collect();
        self.child_model_picker
            .update(cx, |picker, cx| picker.set_excluded(children, cx));
    }

    fn commit_model_identity(&self, provider: ProviderKind, model: &str, cx: &mut Context<Self>) {
        let Some(row) = self
            .identity_rows
            .iter()
            .find(|row| row.provider == provider && row.model == model)
        else {
            return;
        };
        let value = row.identity.read(cx).value().to_string();
        let model = model.to_string();
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings
                    .orchestrate
                    .model_identities
                    .iter_mut()
                    .find(|entry| entry.provider == provider && entry.model == model)
                {
                    entry.identity = value;
                }
            },
            cx,
        );
    }

    fn commit_child_definition(&self, provider: ProviderKind, model: &str, cx: &mut Context<Self>) {
        let Some(row) = self
            .child_rows
            .iter()
            .find(|row| row.provider == provider && row.model == model)
        else {
            return;
        };
        let description = row.description.read(cx).value().to_string();
        let model = model.to_string();
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings
                    .orchestrate
                    .child_models
                    .iter_mut()
                    .find(|entry| entry.provider == provider && entry.model == model)
                {
                    entry.description = description;
                }
            },
            cx,
        );
    }

    fn add_identity(&mut self, option: &ModelOption, window: &mut Window, cx: &mut Context<Self>) {
        let provider = option.provider;
        let model = option.id.clone();
        let identity = self
            .app_state
            .read(cx)
            .settings
            .orchestrate
            .generic_identity
            .clone();
        self.update_settings(
            move |settings| {
                if !settings
                    .orchestrate
                    .model_identities
                    .iter()
                    .any(|entry| entry.provider == provider && entry.model == model)
                {
                    settings
                        .orchestrate
                        .model_identities
                        .push(OrchestratorIdentity {
                            provider,
                            model,
                            identity,
                        });
                }
            },
            cx,
        );
        self.rebuild_rows(window, cx);
        cx.notify();
    }

    fn remove_identity(
        &mut self,
        provider: ProviderKind,
        model: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = model.to_string();
        self.update_settings(
            move |settings| {
                settings
                    .orchestrate
                    .model_identities
                    .retain(|entry| !(entry.provider == provider && entry.model == model));
            },
            cx,
        );
        self.rebuild_rows(window, cx);
        cx.notify();
    }

    fn add_child(&mut self, option: &ModelOption, window: &mut Window, cx: &mut Context<Self>) {
        let profile = OrchestrateChildModel {
            provider: option.provider,
            model: option.id.clone(),
            enabled: true,
            default_effort: option.default_effort.clone(),
            description: OrchestrateSettings::builtin_child_definition(option.provider, &option.id)
                .unwrap_or_default()
                .to_string(),
        };
        self.update_settings(
            move |settings| {
                if !settings
                    .orchestrate
                    .child_models
                    .iter()
                    .any(|entry| entry.provider == profile.provider && entry.model == profile.model)
                {
                    settings.orchestrate.child_models.push(profile);
                }
            },
            cx,
        );
        self.rebuild_rows(window, cx);
        cx.notify();
    }

    fn remove_child(
        &mut self,
        provider: ProviderKind,
        model: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = model.to_string();
        self.update_settings(
            move |settings| {
                settings
                    .orchestrate
                    .child_models
                    .retain(|entry| !(entry.provider == provider && entry.model == model));
            },
            cx,
        );
        self.rebuild_rows(window, cx);
        cx.notify();
    }

    fn set_child_enabled(
        &self,
        provider: ProviderKind,
        model: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let model = model.to_string();
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings
                    .orchestrate
                    .child_models
                    .iter_mut()
                    .find(|entry| entry.provider == provider && entry.model == model)
                {
                    entry.enabled = enabled;
                }
            },
            cx,
        );
    }

    fn reset_generic_identity(&self, window: &mut Window, cx: &mut Context<Self>) {
        let value = OrchestrateSettings::builtin_generic_identity().to_string();
        let persisted = value.clone();
        self.update_settings(
            move |settings| settings.orchestrate.generic_identity = persisted,
            cx,
        );
        self.generic_identity
            .update(cx, |input, cx| input.set_value(value, window, cx));
    }

    fn reset_model_identity(
        &self,
        provider: ProviderKind,
        model: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = OrchestrateSettings::builtin_identity_for(provider, model).to_string();
        let persisted = value.clone();
        let model_key = model.to_string();
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings
                    .orchestrate
                    .model_identities
                    .iter_mut()
                    .find(|entry| entry.provider == provider && entry.model == model_key)
                {
                    entry.identity = persisted;
                }
            },
            cx,
        );
        if let Some(row) = self
            .identity_rows
            .iter()
            .find(|row| row.provider == provider && row.model == model)
        {
            row.identity
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }

    fn reset_child_definition(
        &self,
        provider: ProviderKind,
        model: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = OrchestrateSettings::builtin_child_definition(provider, model)
            .unwrap_or_default()
            .to_string();
        let persisted = value.clone();
        let model_key = model.to_string();
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings
                    .orchestrate
                    .child_models
                    .iter_mut()
                    .find(|entry| entry.provider == provider && entry.model == model_key)
                {
                    entry.description = persisted;
                }
            },
            cx,
        );
        if let Some(row) = self
            .child_rows
            .iter()
            .find(|row| row.provider == provider && row.model == model)
        {
            row.description
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }

    fn model_name(&self, provider: ProviderKind, model: &str, cx: &App) -> String {
        self.identity_model_picker
            .read(cx)
            .display_name(provider, model, cx)
    }

    fn render_intro(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .gap_1()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::Info).small())
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_medium()
                            .child(tcode_i18n::tr!("orchestrate.all_models.title")),
                    ),
            )
            .child(
                div()
                    .pl_6()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("orchestrate.all_models.description")),
            )
            .into_any_element()
    }

    fn section_heading(
        &self,
        title: impl Into<gpui::SharedString>,
        description: impl Into<gpui::SharedString>,
        action: Option<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .items_end()
            .gap_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().text_size(px(15.)).font_semibold().child(title.into()))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(description.into()),
                    ),
            )
            .when_some(action, |this, action| this.child(action))
            .into_any_element()
    }

    fn render_identities(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut section =
            v_flex()
                .w_full()
                .gap_3()
                .child(self.section_heading(
                    tcode_i18n::tr!("orchestrate.identity.title"),
                    tcode_i18n::tr!("orchestrate.identity.description"),
                    None,
                    cx,
                ))
                .child(
                    v_flex()
                        .w_full()
                        .gap_1p5()
                        .p_3()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .child(
                                    div().flex_1().text_size(px(13.)).font_medium().child(
                                        tcode_i18n::tr!("orchestrate.generic_identity.title"),
                                    ),
                                )
                                .child(
                                    Button::new("reset-generic-orchestrator-identity")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Undo)
                                        .label(tcode_i18n::tr!("orchestrate.restore_default"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reset_generic_identity(window, cx);
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(tcode_i18n::tr!("orchestrate.generic_identity.description")),
                        )
                        .child(Input::new(&self.generic_identity)),
                )
                .child(self.section_heading(
                    tcode_i18n::tr!("orchestrate.model_identity.title"),
                    tcode_i18n::tr!("orchestrate.model_identity.description"),
                    Some(self.identity_model_picker.clone().into_any_element()),
                    cx,
                ));

        if self.identity_rows.is_empty() {
            section = section.child(
                div()
                    .w_full()
                    .p_4()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("orchestrate.model_identity.empty")),
            );
        }
        for (index, row) in self.identity_rows.iter().enumerate() {
            let name = self.model_name(row.provider, &row.model, cx);
            let provider = row.provider;
            let model = row.model.clone();
            let reset_model = row.model.clone();
            section = section.child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(provider_glyph(provider).small())
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_size(px(13.)).font_medium().child(name))
                                    .child(
                                        div()
                                            .font_family("monospace")
                                            .text_size(px(11.))
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "{} · {}",
                                                provider_label(provider),
                                                row.model
                                            )),
                                    ),
                            )
                            .child(
                                Button::new(("reset-orchestrator-identity", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Undo)
                                    .label(tcode_i18n::tr!("orchestrate.restore_default"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.reset_model_identity(
                                            provider,
                                            &reset_model,
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(("remove-orchestrator-identity", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Delete)
                                    .tooltip(tcode_i18n::tr!(
                                        "orchestrate.model_identity.use_generic"
                                    ))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_identity(provider, &model, window, cx);
                                    })),
                            ),
                    )
                    .child(Input::new(&row.identity)),
            );
        }
        section.into_any_element()
    }

    fn render_children(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.app_state.read(cx).settings.orchestrate.clone();
        let mut section = v_flex().w_full().gap_3().child(self.section_heading(
            tcode_i18n::tr!("orchestrate.children.title"),
            tcode_i18n::tr!("orchestrate.children.description"),
            Some(self.child_model_picker.clone().into_any_element()),
            cx,
        ));
        if self.child_rows.is_empty() {
            return section
                .child(
                    div()
                        .w_full()
                        .p_4()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().danger)
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(tcode_i18n::tr!("orchestrate.children.empty")),
                )
                .into_any_element();
        }

        if !settings.child_models.iter().any(|profile| profile.enabled) {
            section = section.child(
                div()
                    .w_full()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().warning)
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("orchestrate.children.none_enabled")),
            );
        }

        let mut list = v_flex()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden();
        for (index, row) in self.child_rows.iter().enumerate() {
            let Some(profile) = settings.child_model(row.provider, &row.model) else {
                continue;
            };
            let provider = row.provider;
            let model = row.model.clone();
            let toggle_model = row.model.clone();
            let reset_model = row.model.clone();
            let name = self.model_name(provider, &row.model, cx);
            list = list.child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_3()
                    .when(index > 0, |this| {
                        this.border_t_1().border_color(cx.theme().border)
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(provider_glyph(provider).small())
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_size(px(13.)).font_medium().child(name))
                                    .child(
                                        div()
                                            .font_family("monospace")
                                            .text_size(px(11.))
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "{} · {}",
                                                provider_label(provider),
                                                row.model
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if profile.enabled {
                                        tcode_i18n::tr!("orchestrate.children.enabled")
                                    } else {
                                        tcode_i18n::tr!("orchestrate.children.disabled")
                                    }),
                            )
                            .child(
                                Switch::new(("orchestrate-child-enabled", index))
                                    .checked(profile.enabled)
                                    .tooltip(if profile.enabled {
                                        tcode_i18n::tr!("orchestrate.children.disable")
                                    } else {
                                        tcode_i18n::tr!("orchestrate.children.enable")
                                    })
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        this.set_child_enabled(
                                            provider,
                                            &toggle_model,
                                            *checked,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(("reset-orchestrate-child", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Undo)
                                    .label(tcode_i18n::tr!("orchestrate.restore_default"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.reset_child_definition(
                                            provider,
                                            &reset_model,
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(("remove-orchestrate-child", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Delete)
                                    .tooltip(tcode_i18n::tr!("orchestrate.children.remove"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_child(provider, &model, window, cx);
                                    })),
                            ),
                    )
                    .child(Input::new(&row.description).small()),
            );
        }
        section.child(list).into_any_element()
    }
}

impl Render for OrchestrateSettingsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap_6()
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("settings.orchestrate_section")),
            )
            .child(self.render_intro(cx))
            .child(self.render_identities(cx))
            .child(self.render_children(cx))
    }
}
