//! Settings → Collaboration: the collaborations, what is known of each model they hand
//! work to, and the templates their mandates are written from.

use std::rc::Rc;

use crate::overlay::{DialogActions, Notification, OverlayExt as _};
use crate::scroll::ScrollableElement as _;
use crate::store::{StoreChange, TopicKind, WorkspaceStore};
use crate::theme::ActiveTheme as _;
use crate::time::humanize_ago;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use tcode_core::collaboration::{
    Collaboration, CollaborationMember, ModelGuide, ModelRef, Templates,
};
use tcode_protocol::CommandResponse;

use crate::provider_card::profile_glyph;
use crate::provider_model_picker::{ModelOption, ModelSelected, ProviderModelPicker};

/// A model's name, as the account's catalog gives it.
fn model_name(store: &WorkspaceStore, model: &ModelRef) -> String {
    store
        .picker_models_for_profile(model.profile())
        .into_iter()
        .find(|row| row.id == model.model)
        .map_or_else(|| model.model.clone(), |row| row.name)
}

/// A model as a capsule: its account's glyph and its name. The decision model wears the
/// accent.
fn model_pill(
    store: &WorkspaceStore,
    model: &ModelRef,
    lead: bool,
    compact: bool,
    cx: &App,
) -> gpui::Div {
    let theme = cx.theme();
    let (bg, fg) = if lead {
        (theme.primary.opacity(0.12), theme.primary)
    } else {
        (theme.secondary, theme.foreground)
    };
    h_flex()
        .flex_none()
        .items_center()
        .gap_1p5()
        .h(px(24.))
        .pl(px(8.))
        .pr(px(10.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_secondary(compact, cx)
        .when(lead, |pill| pill.font_medium())
        .child(profile_glyph(
            store,
            model.provider,
            model.profile_id.as_deref(),
        ))
        .child(model_name(store, model))
}

/// "Researched 3 days ago", "Written" or "None yet", as a line.
fn guide_state(guide: Option<&ModelGuide>, now_secs: u64, compact: bool, cx: &App) -> gpui::Div {
    let line = div().flex_none().text_secondary(compact, cx);
    match guide {
        Some(guide) if !guide.text.trim().is_empty() => match guide.researched_at {
            Some(at) => line.text_color(cx.theme().success).child(crate::tr!(
                "collaborations.guide.researched",
                ago = humanize_ago(now_secs.saturating_sub(at))
            )),
            None => line
                .text_color(cx.theme().success)
                .child(crate::tr!("collaborations.guide.written")),
        },
        _ => line.child(crate::tr!("collaborations.guide.none")),
    }
}

fn open_thread(thread_id: String, window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        Box::new(crate::markdown::OpenThreadAnchor {
            thread_id,
            position: None,
        }),
        cx,
    );
}

/// Open the dialog a collaboration is written in: `collaboration` to change it, or a new
/// one. It is written from the templates the host holds, so a host that does not answer
/// with them opens no dialog and says why.
pub(crate) fn open_plan(
    store: &Entity<WorkspaceStore>,
    collaboration: Option<Collaboration>,
    window: &mut Window,
    cx: &mut App,
) {
    let templates = store.update(cx, |store, cx| store.collaboration_templates(cx));
    let store = store.clone();
    window
        .spawn(cx, async move |cx| {
            let templates = templates.await;
            let _ = cx.update(|window, cx| match templates {
                Ok(templates) => PlanDialog::open(store, collaboration, templates, window, cx),
                Err(error) => window.push_notification(Notification::error(error), cx),
            });
        })
        .detach();
}

/// What a dialog left behind does before the window shows something else.
type Leaving = Rc<dyn Fn(&mut App)>;

/// Show the new thread that researches `model`'s guide on `lead`, the request for it in
/// its composer: the user reads it, changes it and sends it, and the thread writes the
/// guide when it is done. `leaving` runs first, for what the dialogs closed would lose.
fn research(
    store: &Entity<WorkspaceStore>,
    model: ModelRef,
    lead: ModelRef,
    leaving: Option<Leaving>,
    window: &mut Window,
    cx: &mut App,
) {
    let request = {
        let store = store.read(cx);
        tcode_core::collaboration::research_prompt(
            &model,
            &model_name(store, &model),
            &store.provider_profile_display_name(model.profile()),
        )
    };
    let opened = store.update(cx, |store, cx| store.research_model(model, lead, cx));
    let store = store.clone();
    window
        .spawn(cx, async move |cx| {
            let opened = opened.await;
            let _ = cx.update(|window, cx| match opened {
                Ok(CommandResponse::SessionId(Some(thread_id))) => {
                    if let Some(leaving) = &leaving {
                        leaving(cx);
                    }
                    window.close_all_dialogs(cx);
                    store.update(cx, |store, cx| {
                        store.append_to_composer(thread_id.clone(), request, cx);
                        store.show_thread(thread_id, cx);
                    });
                }
                Ok(_) => {}
                Err(error) => window.push_notification(Notification::error(error.message), cx),
            });
        })
        .detach();
}

// ---- The page ----

pub struct CollaborationsPanel {
    store: Entity<WorkspaceStore>,
    /// The templates the host holds, once it has answered.
    templates: Option<Templates>,
    /// A fetch of the templates from the repository is on its way.
    refreshing: bool,
    _subscriptions: Vec<Subscription>,
}

impl CollaborationsPanel {
    pub fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.subscribe(&store, |_, _, change: &StoreChange, cx| {
            if matches!(change.topic, TopicKind::Settings | TopicKind::Providers) {
                cx.notify();
            }
        })];
        let mut panel = Self {
            store,
            templates: None,
            refreshing: false,
            _subscriptions: subscriptions,
        };
        panel.load_templates(window, cx);
        panel
    }

    fn load_templates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let templates = self
            .store
            .update(cx, |store, cx| store.collaboration_templates(cx));
        cx.spawn_in(window, async move |this, cx| {
            let templates = templates.await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(templates) = templates {
                    this.templates = Some(templates);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn refresh_templates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        cx.notify();
        let refreshed = self
            .store
            .update(cx, |store, cx| store.refresh_collaboration_templates(cx));
        cx.spawn_in(window, async move |this, cx| {
            let refreshed = refreshed.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.refreshing = false;
                match refreshed {
                    Ok(_) => this.load_templates(window, cx),
                    Err(error) => window.push_notification(Notification::error(error.message), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn plans(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let collaborations = store.settings_ref().collaborations.clone();
        let rows: Vec<AnyElement> =
            collaborations
                .iter()
                .map(|collaboration| {
                    let name = if collaboration.name.trim().is_empty() {
                        crate::tr!("collaborations.unnamed").into_owned()
                    } else {
                        collaboration.name.clone()
                    };
                    let store = self.store.read(cx);
                    let flow =
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .gap_1p5()
                            .items_center()
                            .child(model_pill(store, &collaboration.lead, true, compact, cx))
                            .child(if collaboration.members.is_empty() {
                                div()
                                    .text_secondary(compact, cx)
                                    .child(crate::tr!("collaborations.members.none_short"))
                                    .into_any_element()
                            } else {
                                Icon::new(IconName::ArrowRight)
                                    .text_color(cx.theme().muted_foreground)
                                    .into_any_element()
                            })
                            .children(collaboration.members.iter().map(|member| {
                                model_pill(store, &member.model, false, compact, cx)
                            }));
                    let selector = format!("collaboration-row-{}", collaboration.id);
                    let opened = collaboration.clone();
                    crate::material::accessible_clickable(
                        v_flex()
                            .w_full()
                            .px(px(crate::material::CARD_INSET))
                            .py(px(12.))
                            .gap(cx.theme().tokens.spacing.sm),
                        SharedString::from(format!("collaboration-{}", collaboration.id)),
                        Role::Button,
                        SharedString::from(name.clone()),
                        cx,
                    )
                    .debug_selector(move || selector.clone())
                    .cursor_pointer()
                    .relative()
                    .child(crate::material::row_hover(px(0.), cx))
                    .child(
                        div()
                            .text_body(compact, cx)
                            .font_medium()
                            .child(crate::widgets::fade_text(name)),
                    )
                    .child(flow)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        open_plan(&this.store, Some(opened.clone()), window, cx);
                    }))
                    .into_any_element()
                })
                .collect();
        let body = if rows.is_empty() {
            crate::material::group(cx)
                .child(crate::material::empty_row(
                    crate::tr!("collaborations.empty"),
                    cx,
                ))
                .into_any_element()
        } else {
            crate::material::grouped(rows, cx).into_any_element()
        };
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .items_end()
                    .child(
                        crate::material::group_caption(crate::tr!("collaborations.section"), cx)
                            .flex_1(),
                    )
                    .child(
                        div().pb_1().child(
                            Button::new("collaboration-new")
                                .debug_selector(|| "collaboration-new".into())
                                .outline()
                                .small()
                                .icon(IconName::Plus)
                                .label(crate::tr!("collaborations.new"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    open_plan(&this.store, None, window, cx);
                                })),
                        ),
                    ),
            )
            .child(body)
            .child(crate::settings_rows::footnote(
                crate::tr!("collaborations.footnote"),
                compact,
                cx,
            ))
            .into_any_element()
    }

    /// The models collaborations hand work to, each with what is known of how it is best
    /// used, and the decision model that would research it: that of the first
    /// collaboration it works in.
    fn guides(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let settings = store.settings_ref();
        let mut models: Vec<(ModelRef, Option<ModelRef>)> = Vec::new();
        for collaboration in &settings.collaborations {
            for member in &collaboration.members {
                if !models.iter().any(|(model, _)| model == &member.model) {
                    models.push((member.model.clone(), Some(collaboration.lead.clone())));
                }
            }
        }
        for guide in &settings.model_guides {
            if !models.iter().any(|(model, _)| model == &guide.model) {
                models.push((guide.model.clone(), None));
            }
        }
        let now_secs = store.host_now_secs(cx);
        let rows: Vec<AnyElement> = models
            .iter()
            .enumerate()
            .map(|(index, (model, lead))| {
                let store = self.store.read(cx);
                let guide = store.settings_ref().model_guide(model).cloned();
                let name = model_name(store, model);
                let has = guide
                    .as_ref()
                    .is_some_and(|guide| !guide.text.trim().is_empty());
                let research_button = lead.clone().filter(|_| !has).map(|lead| {
                    let (store, model) = (self.store.clone(), model.clone());
                    Button::new(("collaboration-guide-research", index))
                        .outline()
                        .xsmall()
                        .icon(Icon::empty().path("icons/sparkles.svg"))
                        .label(crate::tr!("collaborations.guide.research"))
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            research(&store, model.clone(), lead.clone(), None, window, cx);
                        })
                });
                let (opened, opened_lead) = (model.clone(), lead.clone());
                let entity = self.store.clone();
                crate::material::accessible_clickable(
                    crate::material::group_row(cx),
                    ("collaboration-guide", index),
                    Role::Button,
                    SharedString::from(name.clone()),
                    cx,
                )
                .debug_selector(move || format!("collaboration-guide-{index}"))
                .cursor_pointer()
                .relative()
                .gap_2()
                .child(crate::material::row_hover(px(0.), cx))
                .child(
                    div()
                        .flex_none()
                        .text_body(compact, cx)
                        .child(profile_glyph(
                            store,
                            model.provider,
                            model.profile_id.as_deref(),
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(compact, cx)
                        .child(crate::widgets::fade_text(name)),
                )
                .child(guide_state(guide.as_ref(), now_secs, compact, cx))
                .children(research_button)
                .child(
                    Button::new(("collaboration-guide-edit", index))
                        .debug_selector(move || format!("collaboration-guide-edit-{index}"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Pencil)
                        .label(crate::tr!("collaborations.guide.edit"))
                        .on_click({
                            let (entity, model, lead) =
                                (self.store.clone(), model.clone(), lead.clone());
                            move |_, window, cx| {
                                cx.stop_propagation();
                                GuideDialog::open(
                                    entity.clone(),
                                    model.clone(),
                                    lead.clone(),
                                    None,
                                    window,
                                    cx,
                                );
                            }
                        }),
                )
                .on_click(move |_, window, cx| {
                    GuideDialog::open(
                        entity.clone(),
                        opened.clone(),
                        opened_lead.clone(),
                        None,
                        window,
                        cx,
                    );
                })
                .into_any_element()
            })
            .collect();
        let body = if rows.is_empty() {
            crate::material::group(cx)
                .child(crate::material::empty_row(
                    crate::tr!("collaborations.guide.empty"),
                    cx,
                ))
                .into_any_element()
        } else {
            crate::material::grouped(rows, cx).into_any_element()
        };
        v_flex()
            .w_full()
            .child(crate::material::group_caption(
                crate::tr!("collaborations.guide.section"),
                cx,
            ))
            .child(body)
            .child(crate::settings_rows::footnote(
                crate::tr!("collaborations.guide.footnote"),
                compact,
                cx,
            ))
            .into_any_element()
    }

    fn templates_group(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let (title, detail) = match &self.templates {
            Some(templates) => (
                crate::tr!(
                    "collaborations.templates.count",
                    count = templates.roles.len().to_string()
                )
                .into_owned(),
                crate::tr!(
                    "collaborations.templates.updated",
                    date = templates.updated_at.as_str()
                )
                .into_owned(),
            ),
            None => (
                crate::tr!("collaborations.templates.loading").into_owned(),
                String::new(),
            ),
        };
        v_flex()
            .w_full()
            .child(crate::material::group_caption(
                crate::tr!("collaborations.templates.section"),
                cx,
            ))
            .child(
                crate::material::group(cx).child(
                    crate::settings_rows::row_frame(compact, cx)
                        .child(crate::settings_rows::row_labels(
                            title,
                            (!detail.is_empty()).then(|| detail.into()),
                            None,
                            compact,
                            cx,
                        ))
                        .child(
                            Button::new("collaboration-templates-refresh")
                                .debug_selector(|| "collaboration-templates-refresh".into())
                                .ghost()
                                .outline()
                                .compact()
                                .label(crate::tr!("collaborations.templates.refresh"))
                                .loading(self.refreshing)
                                .disabled(self.refreshing)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.refresh_templates(window, cx);
                                })),
                        ),
                ),
            )
            .child(crate::settings_rows::footnote(
                crate::tr!("collaborations.templates.footnote"),
                compact,
                cx,
            ))
            .into_any_element()
    }
}

impl Render for CollaborationsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        // A decision model starts its collaborators through `tcode_control`.
        let needs_control = (!self.store.read(cx).settings_ref().control.enabled).then(|| {
            crate::material::notice(cx.theme().warning, cx)
                .child(crate::tr!("collaborations.needs_control"))
        });
        v_flex()
            .w_full()
            .gap(crate::material::section_gap(cx))
            .children(needs_control)
            .child(self.plans(compact, cx))
            .child(self.guides(compact, cx))
            .child(self.templates_group(compact, cx))
    }
}

// ---- A collaboration, being written ----

struct MemberDraft {
    id: String,
    model: ModelRef,
    mandate: Entity<TextareaState>,
    /// The reasoning efforts it may be started at, least first.
    efforts: Vec<String>,
}

/// What a button that opens a list reads: its label, then the chevron.
fn menu_label(label: impl Into<SharedString>, cx: &App) -> gpui::Div {
    h_flex()
        .items_center()
        .gap_1()
        .text_secondary(false, cx)
        .text_color(cx.theme().foreground)
        .child(label.into())
        .child(Icon::new(IconName::ChevronDown).text_color(cx.theme().muted_foreground))
}

/// The reasoning efforts a model takes.
struct Efforts {
    /// Each with its label, least first.
    levels: Vec<(String, String)>,
    /// The one the model starts at.
    default: Option<String>,
}

/// The reasoning efforts `model` takes: nothing for a model that takes none.
fn effort_levels(store: &WorkspaceStore, model: &ModelRef) -> Option<Efforts> {
    let spec = store
        .profile_catalog(model.profile())
        .into_iter()
        .find(|spec| spec.id == model.model)?;
    spec.options.into_iter().find_map(|option| match option {
        agent::OptionDescriptor::Select {
            role: agent::OptionRole::Effort,
            options,
            default_value,
            ..
        } => Some(Efforts {
            levels: options
                .into_iter()
                .filter(|level| level.unavailable.is_none())
                .map(|level| (level.value, level.label))
                .collect(),
            default: default_value,
        }),
        _ => None,
    })
}

impl Efforts {
    /// The efforts a collaborator is held to: those chosen for it, and until any is, the
    /// one its model starts at.
    fn held(&self, chosen: &[String]) -> Vec<String> {
        if chosen.is_empty() {
            self.default.iter().cloned().collect()
        } else {
            chosen.to_vec()
        }
    }
}

/// The efforts a collaborator is held to, as its card reads them: one by its label, a
/// run of neighbouring ones by its ends, and others one by one.
fn efforts_label(levels: &[(String, String)], efforts: &[String]) -> String {
    let held: Vec<usize> = levels
        .iter()
        .enumerate()
        .filter(|(_, (value, _))| efforts.contains(value))
        .map(|(index, _)| index)
        .collect();
    let label = |index: &usize| levels[*index].1.clone();
    match held.as_slice() {
        [] => crate::tr!("collaborations.members.effort_default").into_owned(),
        [first, .., last] if last - first + 1 == held.len() => {
            format!("{} – {}", label(first), label(last))
        }
        held => held.iter().map(label).collect::<Vec<_>>().join(" / "),
    }
}

/// The model the picker gave.
fn model_ref(option: &ModelOption) -> ModelRef {
    ModelRef {
        provider: option.provider,
        profile_id: option.profile_id.clone(),
        model: option.id.clone(),
    }
}

/// The collaborators' models, as the picker that adds them leaves them out.
fn taken(members: &[MemberDraft]) -> Vec<(agent::ProviderKind, String, Option<String>)> {
    members
        .iter()
        .map(|member| {
            let model = &member.model;
            (
                model.provider,
                model.model.clone(),
                model.profile_id.clone(),
            )
        })
        .collect()
}

/// The dialog a collaboration is written in. Nothing is saved until Create or Save.
pub(crate) struct PlanDialog {
    store: Entity<WorkspaceStore>,
    id: String,
    /// The collaboration is one the settings hold, not a new one.
    existing: bool,
    /// Whether the model picker lists it among the favorites, which the picker decides.
    favorite: bool,
    name: Entity<InputState>,
    lead: Option<ModelRef>,
    lead_mandate: Entity<TextareaState>,
    members: Vec<MemberDraft>,
    templates: Templates,
    lead_picker: Entity<ProviderModelPicker>,
    /// Adds collaborators, from what Settings → Control lets a thread be started on.
    member_picker: Entity<ProviderModelPicker>,
    /// The models picked since the dialog last drew, which become collaborators there.
    joining: Vec<ModelRef>,
    _subscriptions: Vec<Subscription>,
}

impl PlanDialog {
    fn open(
        store: Entity<WorkspaceStore>,
        collaboration: Option<Collaboration>,
        templates: Templates,
        window: &mut Window,
        cx: &mut App,
    ) {
        let existing = collaboration.is_some();
        let dialog = cx.new(|cx| {
            let name = cx.new(|cx| {
                InputState::new(window, cx).default_value(
                    collaboration
                        .as_ref()
                        .map(|collaboration| collaboration.name.clone())
                        .unwrap_or_default(),
                )
            });
            // A new collaboration starts with the decision model's template.
            let lead_mandate = collaboration.as_ref().map_or_else(
                || templates.lead.clone(),
                |collaboration| collaboration.lead_mandate.clone(),
            );
            let lead_mandate = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 8)
                    .default_value(lead_mandate)
            });
            let lead_picker = cx.new(|cx| {
                ProviderModelPicker::selection(
                    store.clone(),
                    "collaboration-lead-popover",
                    "collaboration-lead-pick",
                    collaboration.as_ref().map(|collaboration| {
                        let lead = &collaboration.lead;
                        (lead.provider, lead.model.clone(), lead.profile_id.clone())
                    }),
                    cx,
                )
                .unchosen(crate::tr!("collaborations.lead.pick"))
            });
            let member_picker = cx.new(|cx| {
                ProviderModelPicker::adding(
                    store.clone(),
                    "collaboration-member-popover",
                    "collaboration-member-add",
                    crate::tr!("collaborations.members.add"),
                    cx,
                )
                .startable()
            });
            let subscriptions = vec![
                cx.subscribe(
                    &lead_picker,
                    |dialog: &mut PlanDialog, _, picked: &ModelSelected, cx| {
                        dialog.lead = Some(model_ref(&picked.0));
                        cx.notify();
                    },
                ),
                // A pick reaches here without the window its mandate's field is made
                // in, so the collaborator joins when the dialog next draws.
                cx.subscribe(
                    &member_picker,
                    |dialog: &mut PlanDialog, _, picked: &ModelSelected, cx| {
                        let model = model_ref(&picked.0);
                        // A model is a collaborator once.
                        if dialog.joining.contains(&model)
                            || dialog.members.iter().any(|member| member.model == model)
                        {
                            return;
                        }
                        dialog.joining.push(model);
                        dialog.members_changed(cx);
                    },
                ),
                cx.subscribe(&store, |_, _, change: &StoreChange, cx| {
                    if matches!(change.topic, TopicKind::Settings | TopicKind::Providers) {
                        cx.notify();
                    }
                }),
            ];
            let members: Vec<MemberDraft> = collaboration
                .iter()
                .flat_map(|collaboration| &collaboration.members)
                .map(|member| MemberDraft {
                    id: member.id.clone(),
                    model: member.model.clone(),
                    mandate: cx.new(|cx| {
                        TextareaState::new(window, cx)
                            .auto_grow(2, 8)
                            .default_value(member.mandate.clone())
                    }),
                    efforts: member.efforts.clone(),
                })
                .collect();
            member_picker.update(cx, |picker, cx| picker.set_taken(taken(&members), cx));
            PlanDialog {
                store,
                id: collaboration.as_ref().map_or_else(
                    || uuid::Uuid::new_v4().to_string(),
                    |collaboration| collaboration.id.clone(),
                ),
                existing,
                favorite: collaboration
                    .as_ref()
                    .is_some_and(|collaboration| collaboration.favorite),
                name,
                lead: collaboration.map(|collaboration| collaboration.lead),
                lead_mandate,
                members,
                templates,
                lead_picker,
                member_picker,
                joining: Vec::new(),
                _subscriptions: subscriptions,
            }
        });
        window.open_dialog(cx, move |builder, _, cx| {
            let content = dialog.clone();
            builder
                .title(if existing {
                    crate::tr!("collaborations.dialog.edit").into_owned()
                } else {
                    crate::tr!("collaborations.dialog.new").into_owned()
                })
                .wide()
                .content(move |body, _, _| body.child(content.clone()))
                .footer(Self::footer(&dialog, cx))
        });
    }

    /// The collaborators changed: the picker that adds them offers what is left.
    fn members_changed(&mut self, cx: &mut Context<Self>) {
        let mut taken = taken(&self.members);
        taken.extend(self.joining.iter().map(|model| {
            (
                model.provider,
                model.model.clone(),
                model.profile_id.clone(),
            )
        }));
        self.member_picker
            .update(cx, |picker, cx| picker.set_taken(taken, cx));
        cx.notify();
    }

    fn footer(dialog: &Entity<Self>, cx: &App) -> AnyElement {
        let (existing, ready) = {
            let dialog = dialog.read(cx);
            (dialog.existing, dialog.lead.is_some())
        };
        let (removing, saving) = (dialog.clone(), dialog.clone());
        DialogActions::new()
            .leading(existing.then(|| {
                Button::new("collaboration-remove")
                    .debug_selector(|| "collaboration-remove".into())
                    .ghost()
                    .danger()
                    .label(crate::tr!("collaborations.remove"))
                    .on_click(move |_, window, cx| {
                        removing.update(cx, |dialog, cx| {
                            let id = dialog.id.clone();
                            dialog
                                .store
                                .update(cx, |store, _| store.remove_collaboration(id));
                        });
                        window.close_dialog(cx);
                    })
            }))
            .child(
                Button::new("collaboration-cancel")
                    .label(crate::tr!("sidebar.cancel"))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("collaboration-save")
                    .debug_selector(|| "collaboration-save".into())
                    .primary()
                    .disabled(!ready)
                    .label(if existing {
                        crate::tr!("collaborations.dialog.save")
                    } else {
                        crate::tr!("collaborations.dialog.create")
                    })
                    .on_click(move |_, window, cx| {
                        saving.update(cx, |dialog, cx| dialog.save(cx));
                        window.close_dialog(cx);
                    }),
            )
            .into_any_element()
    }

    fn save(&self, cx: &mut Context<Self>) {
        let Some(lead) = self.lead.clone() else {
            return;
        };
        let name = self.name.read(cx).value().trim().to_owned();
        let collaboration = Collaboration {
            id: self.id.clone(),
            name: if name.is_empty() {
                crate::tr!("collaborations.new_name").into_owned()
            } else {
                name
            },
            lead,
            lead_mandate: self.lead_mandate.read(cx).value().trim().to_owned(),
            members: self
                .members
                .iter()
                .map(|member| CollaborationMember {
                    id: member.id.clone(),
                    model: member.model.clone(),
                    mandate: member.mandate.read(cx).value().trim().to_owned(),
                    // What its card shows is what it is held to.
                    efforts: match effort_levels(self.store.read(cx), &member.model) {
                        Some(efforts) => efforts.held(&member.efforts),
                        None => member.efforts.clone(),
                    },
                })
                .collect(),
            favorite: self.favorite,
        };
        self.store
            .update(cx, |store, _| store.save_collaboration(collaboration));
    }

    fn caption(
        &self,
        title: impl Into<SharedString>,
        hint: impl Into<SharedString>,
        cx: &App,
    ) -> gpui::Div {
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(div().text_body(false, cx).font_medium().child(title.into()))
            .child(div().text_secondary(false, cx).child(hint.into()))
    }

    /// The list of role templates beside a collaborator: one fills its mandate.
    /// The efforts collaborator `index` may be started at, of those its model takes: a
    /// list that stays open as levels are put in and taken out. Nothing for a model that
    /// takes no effort.
    fn effort_menu(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let member = self.members.get(index)?;
        let efforts = Rc::new(effort_levels(self.store.read(cx), &member.model)?);
        if efforts.levels.is_empty() {
            return None;
        }
        let levels = efforts.levels.clone();
        let dialog = cx.entity();
        let id = SharedString::from(format!("collaboration-effort-{index}"));
        let label = crate::tr!(
            "collaborations.members.effort",
            levels = efforts_label(&levels, &efforts.held(&member.efforts))
        );
        Some(
            crate::material::overlay_popover(id.clone(), cx)
                .trigger(
                    Button::new(SharedString::from(format!("{id}-trigger")))
                        .debug_selector(move || format!("collaboration-effort-{index}"))
                        .ghost()
                        .outline()
                        .xsmall()
                        .aria_label(label.clone())
                        .child(menu_label(label, cx)),
                )
                .content(move |_, _, cx| {
                    let popover = cx.entity();
                    let held: Vec<String> = dialog
                        .read(cx)
                        .members
                        .get(index)
                        .map(|member| efforts.held(&member.efforts))
                        .unwrap_or_default();
                    let rows: Vec<AnyElement> = levels
                        .iter()
                        .enumerate()
                        .map(|(row, (value, label))| {
                            let on = held.contains(value);
                            let (dialog, popover) = (dialog.clone(), popover.clone());
                            let (levels, value) = (levels.clone(), value.clone());
                            let held = held.clone();
                            crate::material::accessible_clickable(
                                crate::material::menu_item_frame(
                                    false,
                                    crate::material::ItemFill::Hover,
                                    cx,
                                ),
                                ("collaboration-effort-row", row),
                                Role::MenuItemCheckBox,
                                label.clone(),
                                cx,
                            )
                            .aria_selected(on)
                            .w_full()
                            .items_center()
                            .text_body(false, cx)
                            .cursor_pointer()
                            .child(div().flex_1().min_w_0().child(label.clone()))
                            .when(on, |row| row.child(Icon::new(IconName::Check)))
                            .on_click(move |_, _, cx| {
                                dialog.update(cx, |dialog, cx| {
                                    // Kept least first, as the model lists them.
                                    let chosen: Vec<String> = levels
                                        .iter()
                                        .map(|(level, _)| level)
                                        .filter(|level| held.contains(level) != (**level == value))
                                        .cloned()
                                        .collect();
                                    // It runs at some effort: the last one stays.
                                    if let Some(member) = dialog.members.get_mut(index)
                                        && !chosen.is_empty()
                                    {
                                        member.efforts = chosen;
                                    }
                                    cx.notify();
                                });
                                popover.update(cx, |_, cx| cx.notify());
                            })
                            .into_any_element()
                        })
                        .collect();
                    v_flex().w(px(240.)).p_1().gap_0p5().children(rows).child(
                        div()
                            .px_2()
                            .py_1p5()
                            .text_caption(cx)
                            .child(crate::tr!("collaborations.members.effort_hint")),
                    )
                })
                .into_any_element(),
        )
    }

    fn template_menu(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let dialog = cx.entity();
        let roles = self.templates.roles.clone();
        let id = SharedString::from(format!("collaboration-template-{index}"));
        crate::material::overlay_popover(id.clone(), cx)
            .anchor(gpui::Anchor::TopRight)
            .trigger(
                Button::new(SharedString::from(format!("{id}-trigger")))
                    .ghost()
                    .outline()
                    .xsmall()
                    .aria_label(crate::tr!("collaborations.members.template"))
                    .child(menu_label(
                        crate::tr!("collaborations.members.template"),
                        cx,
                    )),
            )
            .content(move |_, _, cx| {
                let popover = cx.entity();
                let row = |row: usize, label: String, muted: bool, text: String| {
                    let (dialog, popover) = (dialog.clone(), popover.clone());
                    crate::material::accessible_clickable(
                        crate::material::menu_item_frame(
                            false,
                            crate::material::ItemFill::Hover,
                            cx,
                        ),
                        ("collaboration-template-row", row),
                        Role::MenuItem,
                        label.clone(),
                        cx,
                    )
                    .w_full()
                    .text_body(false, cx)
                    .when(muted, |row| row.text_color(cx.theme().muted_foreground))
                    .cursor_pointer()
                    .child(label)
                    .on_click(move |_, window, cx| {
                        dialog.update(cx, |dialog, cx| {
                            if let Some(member) = dialog.members.get(index) {
                                let text = text.clone();
                                member
                                    .mandate
                                    .update(cx, |input, cx| input.set_value(text, window, cx));
                            }
                        });
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                    })
                    .into_any_element()
                };
                let mut rows: Vec<AnyElement> = roles
                    .iter()
                    .enumerate()
                    .map(|(n, role)| row(n, role.name.clone(), false, role.mandate.clone()))
                    .collect();
                rows.push(row(
                    roles.len(),
                    crate::tr!("collaborations.members.template_none").into_owned(),
                    true,
                    String::new(),
                ));
                v_flex().w(px(220.)).p_1().gap_0p5().children(rows)
            })
            .into_any_element()
    }
}

impl Render for PlanDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for model in std::mem::take(&mut self.joining) {
            let mandate = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 8));
            // It starts at the effort its model starts at.
            let efforts = effort_levels(self.store.read(cx), &model)
                .and_then(|efforts| efforts.default)
                .into_iter()
                .collect();
            self.members.push(MemberDraft {
                id: uuid::Uuid::new_v4().to_string(),
                model,
                mandate,
                efforts,
            });
        }
        let spacing = cx.theme().tokens.spacing;
        let store = self.store.read(cx);
        let now_secs = store.host_now_secs(cx);

        let name = v_flex()
            .w_full()
            .gap(spacing.xs)
            .child(
                div()
                    .text_body(false, cx)
                    .font_medium()
                    .child(crate::tr!("collaborations.dialog.name")),
            )
            .child(
                Input::new(&self.name).placeholder(crate::tr!("collaborations.name_placeholder")),
            );

        let template = self.templates.lead.clone();
        let lead = v_flex()
            .w_full()
            .gap(spacing.sm)
            .child(self.caption(
                crate::tr!("collaborations.lead.title"),
                crate::tr!("collaborations.lead.description"),
                cx,
            ))
            .child(h_flex().child(self.lead_picker.clone()))
            .child(
                Textarea::new(&self.lead_mandate)
                    .placeholder(crate::tr!("collaborations.lead.mandate_placeholder")),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .child(crate::tr!("collaborations.lead.template_hint")),
                    )
                    .child(
                        Button::new("collaboration-lead-template")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("collaborations.lead.template_restore"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let template = template.clone();
                                this.lead_mandate
                                    .update(cx, |input, cx| input.set_value(template, window, cx));
                            })),
                    ),
            );

        let cards: Vec<AnyElement> = self
            .members
            .iter()
            .enumerate()
            .map(|(index, member)| {
                let store = self.store.read(cx);
                let guide = store.settings_ref().model_guide(&member.model).cloned();
                let (entity, model, lead) =
                    (self.store.clone(), member.model.clone(), self.lead.clone());
                // A research leaves this dialog: what it holds is saved first.
                let saving: Leaving = {
                    let dialog = cx.entity();
                    Rc::new(move |cx: &mut App| dialog.update(cx, |dialog, cx| dialog.save(cx)))
                };
                let selector = format!("collaboration-member-{index}");
                v_flex()
                    .debug_selector(move || selector.clone())
                    .w_full()
                    .gap(spacing.sm)
                    .p(px(10.))
                    .rounded(crate::material::radius_card(cx))
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(model_pill(store, &member.model, false, false, cx))
                            .children(self.effort_menu(index, cx))
                            .child(div().flex_1())
                            .child(self.template_menu(index, cx))
                            .child(
                                Button::new(("collaboration-member-remove", index))
                                    .debug_selector(move || {
                                        format!("collaboration-member-remove-{index}")
                                    })
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip(crate::tr!("collaborations.members.remove"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if index < this.members.len() {
                                            this.members.remove(index);
                                        }
                                        this.members_changed(cx);
                                    })),
                            ),
                    )
                    .child(
                        Textarea::new(&member.mandate)
                            .placeholder(crate::tr!("collaborations.members.mandate_placeholder")),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_none()
                                    .text_secondary(false, cx)
                                    .child(crate::tr!("collaborations.guide.label")),
                            )
                            .child(guide_state(guide.as_ref(), now_secs, false, cx))
                            .child(div().flex_1())
                            .child(
                                Button::new(("collaboration-member-guide", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Pencil)
                                    .label(crate::tr!("collaborations.guide.edit"))
                                    .on_click(move |_, window, cx| {
                                        GuideDialog::open(
                                            entity.clone(),
                                            model.clone(),
                                            lead.clone(),
                                            Some(saving.clone()),
                                            window,
                                            cx,
                                        );
                                    }),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        let none = cards.is_empty().then(|| {
            div()
                .w_full()
                .p(px(10.))
                .rounded(crate::material::radius_card(cx))
                .border_1()
                .border_dashed()
                .border_color(cx.theme().border)
                .text_secondary(false, cx)
                .child(crate::tr!("collaborations.members.none"))
        });
        let members = v_flex()
            .w_full()
            .gap(spacing.sm)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_end()
                    .child(self.caption(
                        crate::tr!("collaborations.members.title"),
                        crate::tr!("collaborations.members.description"),
                        cx,
                    ))
                    .child(self.member_picker.clone()),
            )
            .children(cards)
            .children(none);

        div()
            .id("collaboration-dialog-body")
            .debug_selector(|| "collaboration-dialog-body".into())
            .size_full()
            .overflow_y_scroll_area()
            .child(
                v_flex()
                    .w_full()
                    .gap(spacing.lg)
                    .child(name)
                    .child(lead)
                    .child(members),
            )
    }
}

// ---- What is known of a model ----

/// A model's usage guide: read, written by hand, or asked of a thread.
pub(crate) struct GuideDialog {
    store: Entity<WorkspaceStore>,
    model: ModelRef,
    /// The decision model a research would run on, where one is known.
    lead: Option<ModelRef>,
    /// What the dialog under this one does before a research leaves both.
    leaving: Option<Leaving>,
    text: Entity<TextareaState>,
    /// What the field last wrote: the settings come back later than the keys.
    written: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl GuideDialog {
    fn open(
        store: Entity<WorkspaceStore>,
        model: ModelRef,
        lead: Option<ModelRef>,
        leaving: Option<Leaving>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title = crate::tr!(
            "collaborations.guide.dialog",
            model = model_name(store.read(cx), &model)
        )
        .into_owned();
        let dialog = cx.new(|cx| {
            let stored = store
                .read(cx)
                .settings_ref()
                .model_guide(&model)
                .map(|guide| guide.text.clone())
                .unwrap_or_default();
            let text = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(6, 16)
                    .default_value(stored)
            });
            let subscriptions = vec![
                cx.subscribe(&text, |this: &mut GuideDialog, input, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        this.commit(value, cx);
                    }
                }),
                cx.subscribe_in(
                    &store,
                    window,
                    |this: &mut GuideDialog, _, change: &StoreChange, window, cx| {
                        if change.topic == TopicKind::Settings {
                            this.follow(window, cx);
                            cx.notify();
                        }
                    },
                ),
            ];
            GuideDialog {
                store,
                model,
                lead,
                leaving,
                text,
                written: None,
                _subscriptions: subscriptions,
            }
        });
        window.open_dialog(cx, move |builder, _, cx| {
            let content = dialog.clone();
            builder
                .title(title.clone())
                .wide()
                .content(move |body, _, _| body.child(content.clone()))
                .footer(Self::footer(&dialog, cx))
        });
    }

    fn stored(&self, cx: &App) -> Option<ModelGuide> {
        self.store
            .read(cx)
            .settings_ref()
            .model_guide(&self.model)
            .cloned()
    }

    /// Write the field's text as the guide, keeping who researched it and when.
    fn commit(&mut self, value: String, cx: &mut Context<Self>) {
        let stored = self.stored(cx);
        let held = stored
            .as_ref()
            .map(|guide| guide.text.clone())
            .unwrap_or_default();
        if self.written.as_ref().unwrap_or(&held) == &value {
            return;
        }
        self.written = Some(value.clone());
        let guide = ModelGuide {
            model: self.model.clone(),
            text: value,
            thread_id: stored.as_ref().and_then(|guide| guide.thread_id.clone()),
            researched_at: stored.as_ref().and_then(|guide| guide.researched_at),
        };
        self.store
            .update(cx, |store, _| store.set_model_guide(guide));
    }

    /// Show what the settings hold, where the field is not being typed in: a thread may
    /// have written the guide meanwhile.
    fn follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let held = self.stored(cx).map(|guide| guide.text).unwrap_or_default();
        let input = self.text.read(cx);
        if input.value() == held || gpui::Focusable::focus_handle(input, cx).is_focused(window) {
            return;
        }
        self.written = Some(held.clone());
        self.text
            .update(cx, |input, cx| input.set_value(held, window, cx));
    }

    fn footer(dialog: &Entity<Self>, cx: &App) -> AnyElement {
        let this = dialog.read(cx);
        let store = this.store.read(cx);
        let guide = store.settings_ref().model_guide(&this.model).cloned();
        let has = guide
            .as_ref()
            .is_some_and(|guide| !guide.text.trim().is_empty());
        let research_button = this.lead.clone().map(|lead| {
            let (entity, model) = (this.store.clone(), this.model.clone());
            let leaving = this.leaving.clone();
            let on = model_name(store, &lead);
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("collaboration-guide-research")
                        .debug_selector(|| "collaboration-guide-research".into())
                        .outline()
                        .icon(Icon::empty().path("icons/sparkles.svg"))
                        .label(if has {
                            crate::tr!("collaborations.guide.research_again")
                        } else {
                            crate::tr!("collaborations.guide.research")
                        })
                        .on_click(move |_, window, cx| {
                            research(
                                &entity,
                                model.clone(),
                                lead.clone(),
                                leaving.clone(),
                                window,
                                cx,
                            );
                        }),
                )
                .child(
                    div()
                        .text_secondary(false, cx)
                        .child(crate::tr!("collaborations.guide.on_lead", model = on)),
                )
        });
        DialogActions::new()
            .leading(research_button)
            .child(
                Button::new("collaboration-guide-done")
                    .primary()
                    .label(crate::tr!("collaborations.guide.done"))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .into_any_element()
    }
}

impl Render for GuideDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let guide = self.stored(cx);
        let now_secs = self.store.read(cx).host_now_secs(cx);
        let thread = guide.as_ref().and_then(|guide| guide.thread_id.clone());
        let thread_link = thread.map(|thread_id| {
            Button::new("collaboration-guide-thread")
                .ghost()
                .xsmall()
                .label(crate::tr!("collaborations.guide.open_thread"))
                .on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    open_thread(thread_id.clone(), window, cx);
                })
        });
        v_flex()
            .w_full()
            .gap(cx.theme().tokens.spacing.sm)
            .child(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("collaborations.guide.hint")),
            )
            .child(
                Textarea::new(&self.text)
                    .placeholder(crate::tr!("collaborations.guide.placeholder")),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(guide_state(guide.as_ref(), now_secs, false, cx))
                    .child(div().flex_1())
                    .children(thread_link),
            )
    }
}
