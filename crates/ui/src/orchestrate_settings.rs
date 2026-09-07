//! Settings → Orchestrate: decision peers and execution-model routing profiles.

use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{InputEvent, Textarea, TextareaState};
use crate::widgets::switch::Switch;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use agent::{OptionDescriptor, ProviderKind};
use tcode_core::settings::orchestrate_efforts;

use crate::provider_card::provider_glyph;
use crate::provider_model_picker::{ModelOption, ProviderModelPicker};
use crate::settings::{
    ChildApprovalMode, OrchestrateChildModel, OrchestrateSettings, provider_label,
};
use crate::store::{StoreChange, TopicKind, WorkspaceStore};

struct ChildRowState {
    provider: ProviderKind,
    model: String,
    profile_id: Option<String>,
    description: Entity<TextareaState>,
}

pub struct OrchestrateSettingsPanel {
    store: Entity<WorkspaceStore>,
    child_rows: Vec<ChildRowState>,
    decision_model_picker: Entity<ProviderModelPicker>,
    child_model_picker: Entity<ProviderModelPicker>,
    _subscriptions: Vec<Subscription>,
    input_subscriptions: Vec<Subscription>,
}

impl OrchestrateSettingsPanel {
    pub fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let decision_model_picker = cx.new(|cx| {
            ProviderModelPicker::add(
                store.clone(),
                "orchestrate-add-decision-popover",
                "orchestrate-add-decision",
                crate::tr!("orchestrate.decisions.add"),
                cx,
            )
        });
        let child_model_picker = cx.new(|cx| {
            ProviderModelPicker::add(
                store.clone(),
                "orchestrate-add-child-popover",
                "orchestrate-add-child",
                crate::tr!("orchestrate.children.add"),
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &store,
                window,
                |this, _, change: &StoreChange, window, cx| match change.topic {
                    TopicKind::Settings => {
                        if !this.rows_match_settings(cx) {
                            this.rebuild_rows(window, cx);
                        }
                        cx.notify();
                    }
                    // Fast/effort controls read the provider catalogs.
                    TopicKind::Providers => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(&decision_model_picker, window, |this, _, event, _, cx| {
                this.add_child(&event.0, true, cx);
            }),
            cx.subscribe_in(&child_model_picker, window, |this, _, event, _, cx| {
                this.add_child(&event.0, false, cx);
            }),
        ];
        let mut panel = Self {
            store,
            child_rows: Vec::new(),
            decision_model_picker,
            child_model_picker,
            _subscriptions: subscriptions,
            input_subscriptions: Vec::new(),
        };
        panel.rebuild_rows(window, cx);
        panel
    }

    fn update_models(
        &self,
        decision: bool,
        mutate: impl FnOnce(&mut Vec<OrchestrateChildModel>),
        cx: &mut Context<Self>,
    ) {
        let settings = self.store.read(cx).settings().orchestrate;
        let mut models = if decision {
            settings.decision_models
        } else {
            settings.child_models
        };
        mutate(&mut models);
        self.store.update(cx, |store, _cx| {
            if decision {
                store.set_orchestrate_decision_models(models)
            } else {
                store.set_orchestrate_child_models(models)
            }
        });
    }

    fn profile_location(&self, index: usize, cx: &App) -> (bool, usize) {
        let decisions = self
            .store
            .read(cx)
            .settings()
            .orchestrate
            .decision_models
            .len();
        if index < decisions {
            (true, index)
        } else {
            (false, index - decisions)
        }
    }

    fn update_profile(
        &self,
        index: usize,
        mutate: impl FnOnce(&mut OrchestrateChildModel),
        cx: &mut Context<Self>,
    ) {
        let (decision, index) = self.profile_location(index, cx);
        self.update_models(
            decision,
            move |models| {
                if let Some(entry) = models.get_mut(index) {
                    mutate(entry);
                }
            },
            cx,
        );
    }

    /// Settings patches are echoed back asynchronously, so rows are rebuilt from
    /// the replica only when the set of rows changed — never while typing.
    fn rows_match_settings(&self, cx: &App) -> bool {
        let orchestrate = self.store.read(cx).settings().orchestrate;
        self.child_rows.len() == orchestrate.decision_models.len() + orchestrate.child_models.len()
            && self
                .child_rows
                .iter()
                .zip(
                    orchestrate
                        .decision_models
                        .iter()
                        .chain(&orchestrate.child_models),
                )
                .all(|(row, entry)| {
                    row.provider == entry.provider
                        && row.model == entry.model
                        && row.profile_id == entry.profile_id
                })
    }

    fn rebuild_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input_subscriptions.clear();
        self.child_rows.clear();
        let orchestrate = self.store.read(cx).settings().orchestrate;
        let decision_excluded: Vec<_> = orchestrate
            .decision_models
            .iter()
            .map(|entry| (entry.provider, entry.model.clone()))
            .collect();
        let child_excluded: Vec<_> = orchestrate
            .child_models
            .iter()
            .map(|entry| (entry.provider, entry.model.clone()))
            .collect();
        self.decision_model_picker
            .update(cx, |picker, cx| picker.set_excluded(decision_excluded, cx));
        self.child_model_picker
            .update(cx, |picker, cx| picker.set_excluded(child_excluded, cx));

        for (index, entry) in orchestrate
            .decision_models
            .into_iter()
            .chain(orchestrate.child_models)
            .enumerate()
        {
            let description = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 9)
                    .placeholder(crate::tr!("orchestrate.children.description_placeholder"))
                    .default_value(entry.description)
            });
            self.input_subscriptions
                .push(cx.subscribe(&description, move |this, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.commit_child_definition(index, cx);
                    }
                }));
            self.child_rows.push(ChildRowState {
                provider: entry.provider,
                model: entry.model,
                profile_id: entry.profile_id,
                description,
            });
        }
    }

    fn commit_child_definition(&self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.child_rows.get(index) else {
            return;
        };
        let description = row.description.read(cx).value().to_string();
        let provider = row.provider;
        let model = row.model.clone();
        self.update_profile(
            index,
            move |entry| {
                if entry.provider == provider && entry.model == model {
                    entry.description = description;
                }
            },
            cx,
        );
    }

    fn set_child_fast(&self, index: usize, fast: bool, cx: &mut Context<Self>) {
        self.update_profile(index, move |entry| entry.fast = fast, cx);
    }

    /// Whether the provider catalog declares a fast mode for a model: Claude's
    /// `fastMode` boolean or a Codex `serviceTier` selector offering `fast`.
    fn child_fast_supported(&self, provider: ProviderKind, model: &str, cx: &App) -> bool {
        model_fast_supported(&self.store.read(cx).provider_model_catalog(provider), model)
    }

    fn add_child(&mut self, option: &ModelOption, decision: bool, cx: &mut Context<Self>) {
        let settings = self.store.read(cx).settings().orchestrate;
        let models = if decision {
            &settings.decision_models
        } else {
            &settings.child_models
        };
        if models
            .iter()
            .any(|entry| entry.provider == option.provider && entry.model == option.id)
        {
            return;
        }
        let description = if decision {
            OrchestrateSettings::builtin_decision_definition(option.provider, &option.id)
        } else {
            OrchestrateSettings::builtin_child_definition(option.provider, &option.id)
        };
        let profile = OrchestrateChildModel {
            provider: option.provider,
            model: option.id.clone(),
            profile_id: option.profile_id.clone(),
            enabled: true,
            fast: false,
            description: description.unwrap_or_default().to_string(),
        };
        self.update_models(
            decision,
            move |models| {
                if !models
                    .iter()
                    .any(|entry| entry.provider == profile.provider && entry.model == profile.model)
                {
                    models.push(profile);
                }
            },
            cx,
        );
    }

    fn remove_child(&mut self, index: usize, cx: &mut Context<Self>) {
        let (decision, index) = self.profile_location(index, cx);
        self.update_models(
            decision,
            move |models| {
                if index < models.len() {
                    models.remove(index);
                }
            },
            cx,
        );
    }

    fn set_child_enabled(&self, index: usize, enabled: bool, cx: &mut Context<Self>) {
        self.update_profile(index, move |entry| entry.enabled = enabled, cx);
    }

    /// Restore the bundled model description without changing routing state.
    fn reset_child_definition(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.store.read(cx).settings();
        let models: Vec<_> = settings
            .orchestrate
            .decision_models
            .into_iter()
            .chain(settings.orchestrate.child_models)
            .collect();
        let (decision, _) = self.profile_location(index, cx);
        let Some(target) = builtin_child_target(&models, index, decision) else {
            return;
        };
        let provider = target.provider;
        let model = target.model.clone();
        let description = target.description;
        let persisted_description = description.clone();
        self.update_profile(
            index,
            move |entry| {
                if entry.provider == provider && entry.model == model {
                    entry.description = persisted_description;
                }
            },
            cx,
        );
        // The description textarea holds its own copy.
        if let Some(row) = self.child_rows.get(index) {
            row.description
                .update(cx, |input, cx| input.set_value(description, window, cx));
        }
    }

    /// The per-item "restore default" affordance, mirroring the General page:
    /// icon-only, ghost, and inline immediately after the row's title.
    fn reset_button(
        &self,
        id: impl Into<ElementId>,
        cx: &mut Context<Self>,
        on_reset: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let label = crate::tr!("settings.reset_item").into_owned();
        Button::new(id)
            .ghost()
            .xsmall()
            .icon(IconName::Undo)
            .tooltip(label.clone())
            .aria_label(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                on_reset(this, window, cx);
            }))
            .into_any_element()
    }

    fn row_title(
        &self,
        title: impl Into<gpui::SharedString>,
        reset: Option<AnyElement>,
    ) -> AnyElement {
        h_flex()
            .items_center()
            .gap_1()
            .child(div().text_size(px(13.)).font_medium().child(title.into()))
            .children(reset)
            .into_any_element()
    }

    fn model_name(
        &self,
        provider: ProviderKind,
        model: &str,
        profile_id: Option<&str>,
        cx: &App,
    ) -> String {
        self.decision_model_picker
            .read(cx)
            .display_name(provider, model, profile_id, cx)
    }

    fn status_note(
        &self,
        accent: gpui::Hsla,
        text: impl Into<gpui::SharedString>,
        cx: &Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .items_stretch()
            .gap_2()
            .child(
                div()
                    .flex_none()
                    .w(px(2.))
                    .my_0p5()
                    .rounded_full()
                    .bg(accent),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .line_height(px(18.))
                    .text_color(cx.theme().foreground)
                    .child(text.into()),
            )
            .into_any_element()
    }

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
                            .child(crate::tr!("orchestrate.all_models.title")),
                    ),
            )
            .child(
                div()
                    .pl(px(20.))
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("orchestrate.all_models.description")),
            )
            .into_any_element()
    }

    fn render_child_approval(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.store.read(cx).settings().orchestrate.child_approval;
        let reset = (selected != ChildApprovalMode::Orchestrator).then(|| {
            self.reset_button("reset-orchestrate-child-approval", cx, |this, _, cx| {
                this.store.update(cx, |store, _cx| {
                    store.set_orchestrate_child_approval(ChildApprovalMode::Orchestrator)
                });
            })
        });
        let selected_label = match selected {
            ChildApprovalMode::Orchestrator => {
                crate::tr!("orchestrate.child_approval.orchestrator")
            }
            ChildApprovalMode::AlwaysAllow => {
                crate::tr!("orchestrate.child_approval.always_allow")
            }
            ChildApprovalMode::Manual => crate::tr!("orchestrate.child_approval.manual"),
        };
        let trigger = Button::new("orchestrate-child-approval-dropdown")
            .ghost()
            .outline()
            .compact()
            .child(
                h_flex()
                    .w(px(180.))
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(px(13.))
                    .child(selected_label)
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );
        let panel = cx.entity();
        let dropdown = crate::material::overlay_popover("orchestrate-child-approval-popover")
            .trigger(trigger)
            .content(move |_, _, cx| {
                let option = |mode: ChildApprovalMode,
                              id: &'static str,
                              label: gpui::SharedString,
                              cx: &mut Context<gpui_base::PopoverState>|
                 -> AnyElement {
                    let panel = panel.clone();
                    let popover = cx.entity();
                    h_flex()
                        .id(id)
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .rounded(crate::material::radius_button())
                        .text_size(px(13.))
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().accent))
                        .child(div().flex_1().child(label))
                        .when(mode == selected, |row| {
                            row.child(Icon::new(IconName::Check).xsmall())
                        })
                        .on_click(move |_, window, cx| {
                            panel.update(cx, |panel, cx| {
                                panel.store.update(cx, |store, _cx| {
                                    store.set_orchestrate_child_approval(mode)
                                });
                            });
                            popover.update(cx, |state, cx| state.dismiss(window, cx));
                        })
                        .into_any_element()
                };
                v_flex()
                    .p_1()
                    .min_w(px(180.))
                    .gap_0p5()
                    .child(option(
                        ChildApprovalMode::Orchestrator,
                        "orchestrate-child-approval-orchestrator",
                        crate::tr!("orchestrate.child_approval.orchestrator")
                            .into_owned()
                            .into(),
                        cx,
                    ))
                    .child(option(
                        ChildApprovalMode::AlwaysAllow,
                        "orchestrate-child-approval-always-allow",
                        crate::tr!("orchestrate.child_approval.always_allow")
                            .into_owned()
                            .into(),
                        cx,
                    ))
                    .child(option(
                        ChildApprovalMode::Manual,
                        "orchestrate-child-approval-manual",
                        crate::tr!("orchestrate.child_approval.manual")
                            .into_owned()
                            .into(),
                        cx,
                    ))
            });

        crate::material::group(cx)
            .child(
                h_flex()
                    .w_full()
                    .min_h(px(56.))
                    .px_3()
                    .py_2()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(self.row_title(
                                crate::tr!("orchestrate.child_approval.title").into_owned(),
                                reset,
                            ))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(crate::tr!("orchestrate.child_approval.description")),
                            ),
                    )
                    .child(dropdown),
            )
            .into_any_element()
    }

    fn render_auto_archive(&self, cx: &mut Context<Self>) -> AnyElement {
        let checked = self
            .store
            .read(cx)
            .settings()
            .orchestrate
            .archive_on_complete;
        let reset = (!checked).then(|| {
            self.reset_button("reset-orchestrate-auto-archive", cx, |this, _, cx| {
                this.store.update(cx, |store, _cx| {
                    store.set_orchestrate_archive_on_complete(true)
                });
            })
        });
        crate::material::group(cx)
            .child(
                h_flex()
                    .w_full()
                    .min_h(px(56.))
                    .px_3()
                    .py_2()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(self.row_title(
                                crate::tr!("orchestrate.auto_archive.title").into_owned(),
                                reset,
                            ))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(crate::tr!("orchestrate.auto_archive.description")),
                            ),
                    )
                    .child(
                        Switch::new("orchestrate-auto-archive")
                            .checked(checked)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.store.update(cx, |store, _cx| {
                                    store.set_orchestrate_archive_on_complete(checked)
                                });
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_child_worktrees(&self, cx: &mut Context<Self>) -> AnyElement {
        let checked = self.store.read(cx).settings().orchestrate.child_worktrees;
        let reset = checked.then(|| {
            self.reset_button("reset-orchestrate-child-worktrees", cx, |this, _, cx| {
                this.store.update(cx, |store, _cx| {
                    store.set_orchestrate_child_worktrees(false)
                });
            })
        });
        crate::material::group(cx)
            .child(
                h_flex()
                    .w_full()
                    .min_h(px(56.))
                    .px_3()
                    .py_2()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(self.row_title(
                                crate::tr!("orchestrate.child_worktrees.title").into_owned(),
                                reset,
                            ))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(crate::tr!("orchestrate.child_worktrees.description")),
                            ),
                    )
                    .child(
                        Switch::new("orchestrate-child-worktrees")
                            .checked(checked)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.store.update(cx, |store, _cx| {
                                    store.set_orchestrate_child_worktrees(checked)
                                });
                            })),
                    ),
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

    fn render_children(&self, decision: bool, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.store.read(cx).settings().orchestrate;
        let offset = if decision {
            0
        } else {
            settings.decision_models.len()
        };
        let count = if decision {
            settings.decision_models.len()
        } else {
            settings.child_models.len()
        };
        let models: Vec<_> = settings
            .decision_models
            .into_iter()
            .chain(settings.child_models)
            .collect();
        let profiles = &models[offset..offset + count];
        let mut section = v_flex().w_full().gap_3().child(
            self.section_heading(
                if decision {
                    crate::tr!("orchestrate.decisions.title")
                } else {
                    crate::tr!("orchestrate.children.title")
                },
                if decision {
                    crate::tr!("orchestrate.decisions.description")
                } else {
                    crate::tr!("orchestrate.children.description")
                },
                Some(
                    if decision {
                        self.decision_model_picker.clone()
                    } else {
                        self.child_model_picker.clone()
                    }
                    .into_any_element(),
                ),
                cx,
            ),
        );
        if profiles.is_empty() {
            return section
                .child(self.status_note(
                    cx.theme().danger,
                    if decision {
                        crate::tr!("orchestrate.decisions.empty")
                    } else {
                        crate::tr!("orchestrate.children.empty")
                    },
                    cx,
                ))
                .into_any_element();
        }

        if !profiles.iter().any(|profile| profile.enabled) {
            section = section.child(self.status_note(
                cx.theme().warning,
                if decision {
                    crate::tr!("orchestrate.decisions.none_enabled")
                } else {
                    crate::tr!("orchestrate.children.none_enabled")
                },
                cx,
            ));
        }

        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, row) in self.child_rows.iter().enumerate().skip(offset).take(count) {
            let Some(profile) = models.get(index) else {
                continue;
            };
            let provider = row.provider;
            let name = self.model_name(provider, &row.model, row.profile_id.as_deref(), cx);
            let subtitle = if let Some(id) = row.profile_id.as_deref() {
                let profile_settings = self.store.read(cx).provider_profile_settings(id);
                let profile_name = profile_settings
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(id);
                format!(
                    "{} · {} · {}",
                    provider_label(provider),
                    row.model,
                    profile_name
                )
            } else {
                format!("{} · {}", provider_label(provider), row.model)
            };
            let reset = builtin_child_target(&models, index, decision)
                .filter(|target| target.description != profile.description)
                .map(|_| {
                    self.reset_button(
                        ("reset-orchestrate-child", index),
                        cx,
                        move |this, window, cx| this.reset_child_definition(index, window, cx),
                    )
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
                                    .child(self.row_title(name, reset))
                                    .child(
                                        div()
                                            .font_family("monospace")
                                            .text_size(px(11.))
                                            .text_color(cx.theme().muted_foreground)
                                            .child(subtitle),
                                    ),
                            )
                            .child({
                                let (bg, fg) = if profile.enabled {
                                    (
                                        cx.theme().success.opacity(0.12),
                                        cx.theme().success_foreground,
                                    )
                                } else {
                                    (cx.theme().muted, cx.theme().muted_foreground)
                                };
                                let label = if decision && profile.enabled {
                                    crate::tr!("orchestrate.decisions.enabled")
                                } else if decision {
                                    crate::tr!("orchestrate.decisions.disabled")
                                } else if profile.enabled {
                                    crate::tr!("orchestrate.children.enabled")
                                } else {
                                    crate::tr!("orchestrate.children.disabled")
                                };
                                crate::material::semantic_chip(label, bg, fg)
                            })
                            .child(
                                Switch::new(("orchestrate-child-enabled", index))
                                    .checked(profile.enabled)
                                    .tooltip(if decision && profile.enabled {
                                        crate::tr!("orchestrate.decisions.disable")
                                    } else if decision {
                                        crate::tr!("orchestrate.decisions.enable")
                                    } else if profile.enabled {
                                        crate::tr!("orchestrate.children.disable")
                                    } else {
                                        crate::tr!("orchestrate.children.enable")
                                    })
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        this.set_child_enabled(index, *checked, cx);
                                    })),
                            )
                            .child(
                                Button::new(("remove-orchestrate-child", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Delete)
                                    .tooltip(crate::tr!("orchestrate.children.remove"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_child(index, cx);
                                    })),
                            ),
                    )
                    .child({
                        let catalog = self.store.read(cx).provider_model_catalog(provider);
                        let unavailable =
                            !catalog.is_empty() && !catalog.iter().any(|spec| spec.id == row.model);
                        let choices = orchestrate_efforts(provider, &row.model, &catalog, decision);
                        let efforts = if unavailable {
                            crate::tr!("orchestrate.children.model_unavailable").into_owned()
                        } else if choices.is_empty() {
                            if decision {
                                crate::tr!("orchestrate.decisions.effort_unavailable").into_owned()
                            } else {
                                crate::tr!("orchestrate.children.effort_default").into_owned()
                            }
                        } else {
                            choices.join(" · ")
                        };
                        let fast_supported = self.child_fast_supported(provider, &row.model, cx);
                        h_flex()
                            .gap_4()
                            .items_end()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .items_start()
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(cx.theme().muted_foreground)
                                            .child(crate::tr!("orchestrate.children.effort_label")),
                                    )
                                    .child(div().text_size(px(12.)).child(efforts))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(cx.theme().muted_foreground)
                                            .child(crate::tr!("orchestrate.children.effort_hint")),
                                    ),
                            )
                            // Keep a stored `fast` visible even when the catalog no
                            // longer declares support, so it can be switched off.
                            .when(fast_supported || profile.fast, |row| {
                                row.child(
                                    v_flex()
                                        .gap_1()
                                        .items_start()
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(cx.theme().muted_foreground)
                                                .child(crate::tr!(
                                                    "orchestrate.children.fast_label"
                                                )),
                                        )
                                        .child(
                                            Switch::new(("orchestrate-child-fast", index))
                                                .checked(profile.fast)
                                                .tooltip(crate::tr!(
                                                    "orchestrate.children.fast_hint"
                                                ))
                                                .on_click(cx.listener(
                                                    move |this, checked: &bool, _, cx| {
                                                        this.set_child_fast(index, *checked, cx);
                                                    },
                                                )),
                                        ),
                                )
                            })
                    })
                    .child(
                        Textarea::new(&row.description)
                            .text_sm()
                            .rounded(crate::material::radius_input()),
                    )
                    .into_any_element(),
            );
        }
        section
            .child(crate::material::grouped(rows, cx))
            .into_any_element()
    }
}

fn model_fast_supported(catalog: &[agent::ModelSpec], model: &str) -> bool {
    catalog
        .iter()
        .find(|spec| spec.id == model)
        .into_iter()
        .flat_map(|spec| &spec.options)
        .any(|option| match option {
            OptionDescriptor::Boolean { id, .. } => id == "fastMode",
            OptionDescriptor::Select { id, options, .. } => {
                id == "serviceTier"
                    && options.iter().any(|choice| {
                        choice.value == "fast" || choice.label.eq_ignore_ascii_case("fast")
                    })
            }
        })
}

/// The bundled description for this model and role, independent of endpoint.
fn builtin_child_target(
    rows: &[OrchestrateChildModel],
    index: usize,
    decision: bool,
) -> Option<OrchestrateChildModel> {
    let row = rows.get(index)?;
    let defaults = OrchestrateSettings::default();
    let defaults = if decision {
        defaults.decision_models
    } else {
        defaults.child_models
    };
    defaults
        .into_iter()
        .find(|entry| entry.provider == row.provider && entry.model == row.model)
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
                    .child(crate::tr!("settings.orchestrate_section")),
            )
            .child(self.render_intro(cx))
            .child(self.render_child_approval(cx))
            .child(self.render_child_worktrees(cx))
            .child(self.render_auto_archive(cx))
            .child(self.render_children(true, cx))
            .child(self.render_children(false, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::SelectOption;

    #[test]
    fn codex_fast_support_accepts_the_model_list_priority_tier() {
        let catalog = vec![agent::ModelSpec {
            id: "gpt-6-astra".into(),
            display_name: "GPT-6 Astra".into(),
            is_default: true,
            options: vec![OptionDescriptor::Select {
                id: "serviceTier".into(),
                label: "Service Tier".into(),
                options: vec![SelectOption {
                    value: "priority".into(),
                    label: "Fast".into(),
                    description: None,
                }],
                default_value: Some("default".into()),
            }],
        }];

        assert!(model_fast_supported(&catalog, "gpt-6-astra"));
    }

    #[test]
    fn bundled_astra_restore_target_is_role_aware() {
        let settings = OrchestrateSettings::default();
        let decision_count = settings.decision_models.len();
        let rows: Vec<_> = settings
            .decision_models
            .into_iter()
            .chain(settings.child_models)
            .collect();

        let peer = builtin_child_target(&rows, 0, true).unwrap();
        let executor = builtin_child_target(&rows, decision_count, false).unwrap();
        assert_eq!(peer.model, "gpt-6-astra");
        assert_eq!(executor.model, "gpt-6-astra");
        assert_ne!(peer.description, executor.description);
        assert!(executor.description.contains("Default to low effort"));
    }
}
