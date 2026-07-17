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
    effort: Entity<InputState>,
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

        for (index, entry) in orchestrate.child_models.into_iter().enumerate() {
            let effort = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(tcode_i18n::tr!("orchestrate.children.effort_placeholder"))
                    .default_value(entry.effort.clone().unwrap_or_default())
            });
            let description = cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .auto_grow(3, 9)
                    .placeholder(tcode_i18n::tr!(
                        "orchestrate.children.description_placeholder"
                    ))
                    .default_value(entry.description)
            });
            self.input_subscriptions
                .push(cx.subscribe(&effort, move |this, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.commit_child_effort(index, cx);
                    }
                }));
            self.input_subscriptions
                .push(cx.subscribe(&description, move |this, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.commit_child_definition(index, cx);
                    }
                }));
            self.child_rows.push(ChildRowState {
                provider: entry.provider,
                model: entry.model,
                effort,
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

        self.child_model_picker
            .update(cx, |picker, cx| picker.set_excluded(Vec::new(), cx));
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

    fn commit_child_definition(&self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.child_rows.get(index) else {
            return;
        };
        let description = row.description.read(cx).value().to_string();
        let provider = row.provider;
        let model = row.model.clone();
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings.orchestrate.child_models.get_mut(index)
                    && entry.provider == provider
                    && entry.model == model
                {
                    entry.description = description;
                }
            },
            cx,
        );
    }

    fn commit_child_effort(&self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.child_rows.get(index) else {
            return;
        };
        let effort = row.effort.read(cx).value().trim().to_string();
        let effort = (!effort.is_empty()).then_some(effort);
        let provider = row.provider;
        let model = row.model.clone();
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings.orchestrate.child_models.get_mut(index)
                    && entry.provider == provider
                    && entry.model == model
                {
                    entry.effort = effort;
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
            effort: option.effort.clone(),
            description: OrchestrateSettings::builtin_child_definition(
                option.provider,
                &option.id,
                option.effort.as_deref(),
            )
            .unwrap_or_default()
            .to_string(),
        };
        self.update_settings(
            move |settings| {
                if !settings.orchestrate.child_models.iter().any(|entry| {
                    entry.provider == profile.provider
                        && entry.model == profile.model
                        && entry.effort == profile.effort
                }) {
                    settings.orchestrate.child_models.push(profile);
                }
            },
            cx,
        );
        self.rebuild_rows(window, cx);
        cx.notify();
    }

    fn remove_child(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.update_settings(
            move |settings| {
                if index < settings.orchestrate.child_models.len() {
                    settings.orchestrate.child_models.remove(index);
                }
            },
            cx,
        );
        self.rebuild_rows(window, cx);
        cx.notify();
    }

    fn set_child_enabled(&self, index: usize, enabled: bool, cx: &mut Context<Self>) {
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings.orchestrate.child_models.get_mut(index) {
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

    fn reset_child_definition(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self
            .app_state
            .read(cx)
            .settings
            .orchestrate
            .child_models
            .get(index)
        else {
            return;
        };
        let provider = entry.provider;
        let model = entry.model.clone();
        let value = OrchestrateSettings::builtin_child_definition(
            provider,
            &model,
            entry.effort.as_deref(),
        )
        .unwrap_or_default()
        .to_string();
        let persisted = value.clone();
        self.update_settings(
            move |settings| {
                if let Some(entry) = settings.orchestrate.child_models.get_mut(index)
                    && entry.provider == provider
                    && entry.model == model
                {
                    entry.description = persisted;
                }
            },
            cx,
        );
        if let Some(row) = self.child_rows.get(index) {
            row.description
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }

    fn model_name(&self, provider: ProviderKind, model: &str, cx: &App) -> String {
        self.identity_model_picker
            .read(cx)
            .display_name(provider, model, cx)
    }

    /// A status message in the shared rail language: a soft neutral fill with a
    /// floating 2px rail in the semantic color — no colored slab.
    fn status_note(
        &self,
        accent: gpui::Hsla,
        text: impl Into<gpui::SharedString>,
        cx: &Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .items_stretch()
            .rounded(crate::material::radius_card())
            .overflow_hidden()
            .bg(cx.theme().muted.opacity(0.6))
            .child(
                div()
                    .flex_none()
                    .w(px(2.))
                    .ml(px(8.))
                    .my(px(8.))
                    .rounded_full()
                    .bg(accent),
            )
            .child(
                div()
                    .flex_1()
                    .px_3()
                    .py_2p5()
                    .text_size(px(13.))
                    .text_color(cx.theme().foreground)
                    .child(text.into()),
            )
            .into_any_element()
    }

    /// Quiet helper text, System-Settings style: a muted caption under the
    /// section, not a colored callout box.
    fn render_intro(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(IconName::Info).xsmall())
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_medium()
                            .child(tcode_i18n::tr!("orchestrate.all_models.title")),
                    ),
            )
            .child(
                div()
                    .pl(px(20.))
                    .text_size(px(11.))
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
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(description.into()),
                    ),
            )
            .when_some(action, |this, action| this.child(action))
            .into_any_element()
    }

    /// One grouped-list container: popover fill, a single hairline border,
    /// input-radius corners, no shadow.
    fn group(&self, cx: &Context<Self>) -> gpui::Div {
        v_flex()
            .w_full()
            .rounded(crate::material::radius_input())
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .overflow_hidden()
    }

    /// Assemble rows into a group, split by inset hairlines (indented past the
    /// row's left padding, never after the last row).
    fn grouped(&self, rows: Vec<AnyElement>, cx: &Context<Self>) -> gpui::Div {
        let mut group = self.group(cx);
        let last = rows.len().saturating_sub(1);
        for (index, row) in rows.into_iter().enumerate() {
            group = group.child(row);
            if index != last {
                group = group.child(
                    div()
                        .w_full()
                        .pl_3()
                        .child(div().w_full().h(px(1.)).bg(cx.theme().border)),
                );
            }
        }
        group
    }

    fn render_identities(&self, cx: &mut Context<Self>) -> AnyElement {
        let section = v_flex()
            .w_full()
            .gap_3()
            .child(self.section_heading(
                tcode_i18n::tr!("orchestrate.identity.title"),
                tcode_i18n::tr!("orchestrate.identity.description"),
                None,
                cx,
            ))
            // Generic identity: a single-row group holding the header, help and
            // its text area — no slab fill.
            .child(
                self.group(cx).child(
                    v_flex()
                        .w_full()
                        .gap_1p5()
                        .px_3()
                        .py_3()
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
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child(tcode_i18n::tr!("orchestrate.generic_identity.description")),
                        )
                        .child(
                            Input::new(&self.generic_identity)
                                .rounded(crate::material::radius_input()),
                        ),
                ),
            )
            .child(self.section_heading(
                tcode_i18n::tr!("orchestrate.model_identity.title"),
                tcode_i18n::tr!("orchestrate.model_identity.description"),
                Some(self.identity_model_picker.clone().into_any_element()),
                cx,
            ));

        if self.identity_rows.is_empty() {
            return section
                .child(
                    self.group(cx).child(
                        div()
                            .w_full()
                            .px_3()
                            .py_3()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(tcode_i18n::tr!("orchestrate.model_identity.empty")),
                    ),
                )
                .into_any_element();
        }

        // Per-model identities: one grouped list, rows split by inset hairlines.
        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, row) in self.identity_rows.iter().enumerate() {
            let name = self.model_name(row.provider, &row.model, cx);
            let provider = row.provider;
            let model = row.model.clone();
            let reset_model = row.model.clone();
            rows.push(
                v_flex()
                    .w_full()
                    .gap_2()
                    .px_3()
                    .py_3()
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
                    .child(Input::new(&row.identity).rounded(crate::material::radius_input()))
                    .into_any_element(),
            );
        }
        section.child(self.grouped(rows, cx)).into_any_element()
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
                .child(self.status_note(
                    cx.theme().danger,
                    tcode_i18n::tr!("orchestrate.children.empty"),
                    cx,
                ))
                .into_any_element();
        }

        if !settings.child_models.iter().any(|profile| profile.enabled) {
            section = section.child(self.status_note(
                cx.theme().warning,
                tcode_i18n::tr!("orchestrate.children.none_enabled"),
                cx,
            ));
        }

        // Child profiles: one grouped list, rows split by inset hairlines.
        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, row) in self.child_rows.iter().enumerate() {
            let Some(profile) = settings.child_models.get(index) else {
                continue;
            };
            let provider = row.provider;
            let name = self.model_name(provider, &row.model, cx);
            let effort = profile.effort.clone().unwrap_or_else(|| {
                tcode_i18n::tr!("orchestrate.children.effort_default").into_owned()
            });
            rows.push(
                v_flex()
                    .w_full()
                    .gap_2()
                    .px_3()
                    .py_3()
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
                                                "{} · {} · {}",
                                                provider_label(provider),
                                                row.model,
                                                effort,
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .rounded_full()
                                    .when(profile.enabled, |status| {
                                        status
                                            .bg(cx.theme().success.opacity(0.12))
                                            .text_color(cx.theme().success_foreground)
                                    })
                                    .when(!profile.enabled, |status| {
                                        status
                                            .bg(cx.theme().muted)
                                            .text_color(cx.theme().muted_foreground)
                                    })
                                    .text_size(px(11.))
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
                                        this.set_child_enabled(index, *checked, cx);
                                    })),
                            )
                            .child(
                                Button::new(("reset-orchestrate-child", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Undo)
                                    .label(tcode_i18n::tr!("orchestrate.restore_default"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.reset_child_definition(index, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("remove-orchestrate-child", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Delete)
                                    .tooltip(tcode_i18n::tr!("orchestrate.children.remove"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_child(index, window, cx);
                                    })),
                            ),
                    )
                    .child(
                        v_flex()
                            .w(px(160.))
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(tcode_i18n::tr!("orchestrate.children.effort_label")),
                            )
                            .child(
                                Input::new(&row.effort)
                                    .small()
                                    .rounded(crate::material::radius_input()),
                            ),
                    )
                    .child(
                        Input::new(&row.description)
                            .small()
                            .rounded(crate::material::radius_input()),
                    )
                    .into_any_element(),
            );
        }
        section.child(self.grouped(rows, cx)).into_any_element()
    }
}

impl Render for OrchestrateSettingsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap_6()
            .child(
                div()
                    .pl_3()
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
