use super::*;
use crate::control::ActivityDay;
use crate::store::AgentChoice;
use crate::widgets::radio::{RadioList, RadioOption};
use gpui_base::StyledExt as _;
use std::rc::Rc;
use tcode_core::control::{AgentGrant, AgentScope, ModelGrant};
use tcode_core::settings::ChildApprovalMode;
use tcode_protocol::{ControlActivityEntry, ControlActivityFilter, ControlActivityPage};

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// How many `tcode_control` writes the section adds at a time.
const ACTIVITY_PAGE: u32 = 50;

/// What the activity list shows; the entries themselves are read where drawn.
#[derive(Clone, PartialEq)]
enum ActivityState {
    Loading,
    Failed(String),
    Empty,
    NoMatch,
    Entries,
}

pub(super) struct ControlSection {
    activity: Option<Result<ControlActivityPage, String>>,
    activity_task: Option<gpui::Task<()>>,
    /// Latches the one activity query fired when the section opens.
    activity_requested: bool,
    activity_filter: ControlActivityFilter,
    /// A page is the newest `limit` entries rather than the next ones, so a
    /// write arriving between two pages cannot shift one into the other.
    activity_limit: u32,
    /// The search of each account's list of models to add, where the list is long enough
    /// to want one, with what redraws the page as it is typed in.
    agent_search: std::collections::HashMap<String, (Entity<InputState>, Subscription)>,
}

impl Default for ControlSection {
    fn default() -> Self {
        Self {
            activity: None,
            activity_task: None,
            activity_requested: false,
            activity_filter: ControlActivityFilter::default(),
            activity_limit: ACTIVITY_PAGE,
            agent_search: Default::default(),
        }
    }
}

/// How far a capsule's cross reaches past its corner.
const CROSS_OVERHANG: f32 = 5.;

/// From how many models on an account's list of models to add has a search.
const SEARCHED_FROM: usize = 9;

impl AgentChoice {
    /// What the choice lets be added one by one: its models, or for the ACP profile its
    /// agents, each an id and a name.
    fn items(&self) -> &[(String, String)] {
        if self.acp() {
            &self.acp_agents
        } else {
            &self.models
        }
    }

    fn acp(&self) -> bool {
        self.provider == agent::ProviderKind::Acp
    }

    /// The grant that holds all of it: for an account every model, those it lists later
    /// included; for the ACP profile, the agents installed now.
    fn whole(&self) -> AgentGrant {
        AgentGrant {
            profile_id: self.profile_id.clone(),
            models: ModelGrant::Every,
            acp_agents: self.acp_agents.iter().map(|(id, _)| id.clone()).collect(),
        }
    }

    /// The grant that holds the items `ids` and no other.
    fn only(&self, ids: Vec<String>) -> AgentGrant {
        if self.acp() {
            AgentGrant {
                profile_id: self.profile_id.clone(),
                models: ModelGrant::Every,
                acp_agents: ids,
            }
        } else {
            AgentGrant {
                profile_id: self.profile_id.clone(),
                models: ModelGrant::Only(ids),
                acp_agents: Vec::new(),
            }
        }
    }

    /// The items `grant` names one by one, in the order they were added; none where it
    /// holds every model.
    fn added<'a>(&self, grant: &'a AgentGrant) -> &'a [String] {
        match &grant.models {
            _ if self.acp() => &grant.acp_agents,
            ModelGrant::Only(models) => models,
            ModelGrant::Every => &[],
        }
    }

    /// Whether `grant` holds every model of the account, those it lists later included.
    fn every_model(&self, grant: &AgentGrant) -> bool {
        !self.acp() && grant.models == ModelGrant::Every
    }
}

/// The grants a scope holds, one by one: every choice whole where it holds everything.
fn held(scope: &AgentScope, choices: &[AgentChoice]) -> Vec<AgentGrant> {
    match scope {
        AgentScope::Every => choices.iter().map(AgentChoice::whole).collect(),
        AgentScope::Only(grants) => grants.clone(),
    }
}

/// The scope `grants` make: an account left with nothing added is not held, and with every
/// choice whole the scope holds everything, accounts added later included.
fn settled(grants: Vec<AgentGrant>, choices: &[AgentChoice]) -> AgentScope {
    let choice = |grant: &AgentGrant| {
        choices
            .iter()
            .find(|choice| choice.profile_id == grant.profile_id)
    };
    let grants: Vec<AgentGrant> = grants
        .into_iter()
        .filter(|grant| {
            choice(grant)
                .is_none_or(|choice| choice.every_model(grant) || !choice.added(grant).is_empty())
        })
        .collect();
    let whole = choices.iter().all(|choice| {
        grants
            .iter()
            .find(|grant| grant.profile_id == choice.profile_id)
            .is_some_and(|grant| {
                if choice.acp() {
                    choice
                        .acp_agents
                        .iter()
                        .all(|(id, _)| grant.acp_agents.contains(id))
                } else {
                    grant.models == ModelGrant::Every
                }
            })
    });
    if whole {
        AgentScope::Every
    } else {
        AgentScope::Only(grants)
    }
}

/// A model or agent an account holds: a capsule, with the cross that takes it out again.
/// A pointer shows the cross over the capsule's corner while it is on the capsule; a touch
/// layout, which has no hover, keeps it in the capsule. Out of sight the cross still takes
/// a press, which is how a keyboard or an assistive tool reaches it.
fn agent_pill(
    id: SharedString,
    label: String,
    every: bool,
    compact: bool,
    remove: impl Fn(&mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme();
    let (bg, fg) = if every {
        (theme.primary.opacity(0.12), theme.primary)
    } else {
        (theme.secondary, theme.foreground)
    };
    let cross = crate::material::accessible_clickable(
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(if compact { 18. } else { 15. }))
            .rounded_full()
            .bg(theme.muted_foreground)
            .text_color(theme.background),
        SharedString::from(format!("{id}-remove")),
        Role::Button,
        crate::tr!("control.agents.remove", name = label.as_str()).into_owned(),
        cx,
    )
    .debug_selector({
        let id = id.clone();
        move || format!("{id}-remove")
    })
    .cursor_pointer()
    .child(Icon::new(IconName::Close).xsmall())
    .on_click(move |_, _, cx| {
        cx.stop_propagation();
        remove(cx)
    });
    let pill = gpui_base::h_flex()
        .flex_none()
        .items_center()
        .gap_1p5()
        .h(px(24.))
        .px(px(10.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_secondary(compact, cx)
        .when(every, |pill| pill.font_medium())
        .child(label);
    if compact {
        return pill.pr(px(3.)).child(cross).into_any_element();
    }
    let (hovered, on_hover) = crate::motion::hovered(id.clone(), window, cx);
    let shown = gpui_base::motion::transition(
        (id.clone(), "remove-shown"),
        if hovered { 1. } else { 0. },
        gpui_base::motion::Transition::new(CHANGE),
        window,
        cx,
    );
    // The cross sits over the capsule's corner, half outside it. The capsule's hover takes
    // that corner in, so the cross stays while the pointer is on it; the margins give the
    // room back, leaving the capsule where it would be without it.
    div()
        .id(id)
        .relative()
        .flex_none()
        .pt(px(CROSS_OVERHANG))
        .pr(px(CROSS_OVERHANG))
        .mt(px(-CROSS_OVERHANG))
        .mr(px(-CROSS_OVERHANG))
        .on_hover(on_hover)
        .child(pill)
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .opacity(shown)
                .child(cross),
        )
        .into_any_element()
}

impl ControlSection {
    pub(super) fn hidden(&mut self) {
        self.activity_requested = false;
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

impl SettingsPage {
    fn refresh_control_activity(&mut self, cx: &mut Context<Self>) {
        self.control.activity_requested = true;
        let (limit, filter) = (
            self.control.activity_limit,
            self.control.activity_filter.clone(),
        );
        let query = self
            .store
            .update(cx, |store, cx| store.control_activity(limit, filter, cx));
        self.control.activity_task = Some(cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                this.control.activity = Some(result);
                this.control.activity_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn filter_control_activity(
        &mut self,
        change: impl FnOnce(&mut ControlActivityFilter),
        cx: &mut Context<Self>,
    ) {
        change(&mut self.control.activity_filter);
        self.control.activity_limit = ACTIVITY_PAGE;
        self.refresh_control_activity(cx);
    }

    pub(super) fn render_control(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        if !self.control.activity_requested {
            self.refresh_control_activity(cx);
        }
        let settings = self.store.read(cx).settings();
        let control = settings.control.clone();
        let compact = self.window_state.read(cx).compact;

        let enabled_reset = crate::settings_rows::reset_action(
            "reset-control-enabled",
            !control.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_control_enabled(true), cx)
            }),
        );
        let enabled = self.toggle_row(
            "control-enabled",
            crate::tr!("control.settings.enabled"),
            if control.enabled {
                crate::tr!("control.settings.enabled_description")
            } else {
                crate::tr!("control.settings.enabled_off_description")
            },
            control.enabled,
            enabled_reset,
            cx,
            WorkspaceStore::set_control_enabled,
        );

        let machines_reset = crate::settings_rows::reset_action(
            "reset-control-machines",
            !control.machines,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_control_machines(true), cx)
            }),
        );
        let machines = self.toggle_row(
            "control-machines",
            crate::tr!("control.settings.machines"),
            crate::tr!("control.settings.machines_description"),
            control.machines,
            machines_reset,
            cx,
            WorkspaceStore::set_control_machines,
        );

        let visualize_reset = crate::settings_rows::reset_action(
            "reset-visualize-enabled",
            !settings.visualize.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_visualize_enabled(true), cx)
            }),
        );
        let visualize = self.toggle_row(
            "visualize-enabled",
            crate::tr!("control.settings.visualize_enabled"),
            if settings.visualize.enabled {
                crate::tr!("control.settings.visualize_enabled_description")
            } else {
                crate::tr!("control.settings.visualize_enabled_off_description")
            },
            settings.visualize.enabled,
            visualize_reset,
            cx,
            WorkspaceStore::set_visualize_enabled,
        );

        // Everything under the `tcode_control` switch stays as it is while the
        // switch is off, dimmed: turning it back on restores it.
        let dim = gpui_base::motion::transition(
            "control-dim",
            if control.enabled { 1. } else { 0.5 },
            gpui_base::motion::Transition::new(CHANGE),
            window,
            cx,
        );
        let below = v_flex()
            .gap(crate::material::section_gap(cx))
            .child(self.control_agents_group(compact, window, cx))
            .child(self.subagent_group(compact, cx))
            .child(self.control_activity_group(compact, window, cx));

        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(
                crate::material::group(cx)
                    .child(enabled)
                    .child(div().opacity(dim).child(machines))
                    .child(visualize),
            )
            .child(below.opacity(dim))
    }

    /// The accounts and models `tcode_control` may start a thread on. An account is a row;
    /// what it lists is what a thread may be started on, each model a capsule its cross
    /// takes out, and the plus beside the account adds to them.
    fn control_agents_group(
        &mut self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let scope = store.settings_ref().control.agents.clone();
        let choices: Rc<[AgentChoice]> = store.agent_choices().into();
        let grants: Rc<[AgentGrant]> = held(&scope, &choices).into();
        let page = cx.entity();
        // Hold `profile_id` as `grant`, or not at all.
        let set = {
            let (choices, grants, page) = (choices.clone(), grants.clone(), page.clone());
            move |profile_id: &str, grant: Option<AgentGrant>, cx: &mut App| {
                let mut grants: Vec<AgentGrant> = grants.to_vec();
                match (
                    grants
                        .iter()
                        .position(|other| other.profile_id == profile_id),
                    grant,
                ) {
                    (Some(position), Some(grant)) => grants[position] = grant,
                    (Some(position), None) => {
                        grants.remove(position);
                    }
                    (None, Some(grant)) => grants.push(grant),
                    (None, None) => {}
                }
                let scope = settled(grants, &choices);
                page.update(cx, |page, cx| {
                    page.dispatch_settings(move |store| store.set_control_agents(scope), cx)
                })
            }
        };
        let reset =
            crate::settings_rows::reset_action("reset-control-agents", !scope.is_every(), {
                let page = page.clone();
                move |_, _, cx| {
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(
                            |store| store.set_control_agents(AgentScope::Every),
                            cx,
                        )
                    })
                }
            });
        for choice in choices.iter() {
            if choice.items().len() >= SEARCHED_FROM
                && !self.control.agent_search.contains_key(&choice.profile_id)
            {
                let search = cx.new(|cx| InputState::new(window, cx));
                let typed = cx.subscribe(&search, |_, _, event, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                });
                self.control
                    .agent_search
                    .insert(choice.profile_id.clone(), (search, typed));
            }
        }
        let rows: Vec<AnyElement> = choices
            .iter()
            .map(|choice| {
                let grant = grants
                    .iter()
                    .find(|grant| grant.profile_id == choice.profile_id)
                    .cloned();
                let every = grant
                    .as_ref()
                    .is_some_and(|grant| choice.every_model(grant));
                let added: Vec<String> = grant
                    .as_ref()
                    .map(|grant| choice.added(grant).to_vec())
                    .unwrap_or_default();
                let glyph = crate::provider_card::profile_glyph(
                    self.store.read(cx),
                    choice.provider,
                    Some(&choice.profile_id),
                );
                let mut pills: Vec<AnyElement> = Vec::new();
                if every {
                    let (set, profile_id) = (set.clone(), choice.profile_id.clone());
                    pills.push(agent_pill(
                        SharedString::from(format!("control-agent-{}-every", choice.profile_id)),
                        crate::tr!("control.agents.every_model").into_owned(),
                        true,
                        compact,
                        move |cx| set(&profile_id, None, cx),
                        window,
                        cx,
                    ));
                }
                for id in &added {
                    let name = choice
                        .items()
                        .iter()
                        .find(|(item, _)| item == id)
                        .map_or_else(|| id.clone(), |(_, name)| name.clone());
                    let rest: Vec<String> =
                        added.iter().filter(|other| *other != id).cloned().collect();
                    let (set, choice) = (set.clone(), choice.clone());
                    pills.push(agent_pill(
                        SharedString::from(format!("control-agent-{}-{id}", choice.profile_id)),
                        name,
                        false,
                        compact,
                        move |cx| set(&choice.profile_id, Some(choice.only(rest.clone())), cx),
                        window,
                        cx,
                    ));
                }
                let listed = if pills.is_empty() {
                    div()
                        .pt(cx.theme().tokens.spacing.xs)
                        .text_secondary(compact, cx)
                        .child(crate::tr!("control.agents.none"))
                        .into_any_element()
                } else {
                    gpui_base::h_flex()
                        .w_full()
                        .flex_wrap()
                        .gap_2()
                        .pt(cx.theme().tokens.spacing.sm)
                        .children(pills)
                        .into_any_element()
                };
                let selector = format!("control-agent-{}", choice.profile_id);
                v_flex()
                    .debug_selector(move || selector.clone())
                    .w_full()
                    .px(px(crate::material::CARD_INSET))
                    .py(px(10.))
                    .child(
                        gpui_base::h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(div().flex_none().text_body(compact, cx).child(glyph))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_body(compact, cx)
                                    .child(crate::widgets::fade_text(choice.name.clone())),
                            )
                            .child(self.control_agent_add(choice, every, set.clone(), cx)),
                    )
                    .child(listed)
                    .into_any_element()
            })
            .collect();
        v_flex()
            .child(
                gpui_base::h_flex()
                    .items_center()
                    .child(crate::material::group_caption(
                        crate::tr!("control.agents.section"),
                        cx,
                    ))
                    .child(reset),
            )
            .child(crate::material::grouped(rows, cx))
            .child(crate::settings_rows::footnote(
                crate::tr!("control.agents.footnote"),
                compact,
                cx,
            ))
            .into_any_element()
    }

    /// The plus beside an account: the list of what it does not hold yet, which stays open
    /// as its rows are taken one by one, until a click beside it or on the plus again.
    fn control_agent_add(
        &self,
        choice: &AgentChoice,
        every: bool,
        set: impl Fn(&str, Option<AgentGrant>, &mut App) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let profile_id = choice.profile_id.clone();
        let selector = format!("control-agent-add-{profile_id}");
        let trigger = Button::new(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .ghost()
            .outline()
            .xsmall()
            .icon(IconName::Plus)
            .aria_label(crate::tr!(
                "control.agents.add",
                name = choice.name.as_str()
            ))
            .tooltip(if every {
                crate::tr!("control.agents.every_model_held")
            } else {
                crate::tr!("control.agents.add", name = choice.name.as_str())
            })
            .disabled(every);
        if every {
            return trigger.into_any_element();
        }
        let (choice, store) = (choice.clone(), self.store.clone());
        let search = self
            .control
            .agent_search
            .get(&profile_id)
            .map(|(search, _)| search.clone());
        crate::material::overlay_popover(
            SharedString::from(format!("control-agent-add-popover-{profile_id}")),
            cx,
        )
        .anchor(gpui::Anchor::TopRight)
        .trigger(trigger)
        .content(move |_, window, cx| {
            // The list is of what the settings hold now, so a row taken here leaves it.
            let grant = held(
                &store.read(cx).settings_ref().control.agents,
                std::slice::from_ref(&choice),
            )
            .into_iter()
            .find(|grant| grant.profile_id == choice.profile_id);
            let added: Vec<String> = grant
                .as_ref()
                .map(|grant| choice.added(grant).to_vec())
                .unwrap_or_default();
            let left: Vec<(String, String)> = choice
                .items()
                .iter()
                .filter(|(id, _)| !added.contains(id))
                .cloned()
                .collect();
            let query = search
                .as_ref()
                .map(|search| search.read(cx).value().to_lowercase())
                .unwrap_or_default();
            let popover = cx.entity();
            let row = |index: usize,
                       label: String,
                       detail: Option<String>,
                       accent: bool,
                       then_close: bool,
                       next: AgentGrant| {
                let (set, popover, profile_id) =
                    (set.clone(), popover.clone(), choice.profile_id.clone());
                crate::material::accessible_clickable(
                    crate::material::menu_item_frame(false, crate::material::ItemFill::Hover, cx),
                    ("control-agent-add-row", index),
                    Role::MenuItem,
                    label.clone(),
                    cx,
                )
                .debug_selector(move || format!("control-agent-add-row-{index}"))
                .flex_none()
                .w_full()
                .items_center()
                .text_body(false, cx)
                .cursor_pointer()
                .when(accent, |row| row.text_color(cx.theme().primary))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(label)),
                        )
                        .children(detail.map(|detail| {
                            div()
                                .overflow_hidden()
                                .text_meta_mono(false, cx)
                                .child(crate::widgets::fade_text(detail))
                        })),
                )
                .when(!then_close, |row| {
                    row.child(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
                })
                .on_click(move |_, window, cx| {
                    set(&profile_id, Some(next.clone()), cx);
                    popover.update(cx, |state, cx| {
                        if then_close {
                            state.dismiss(window, cx);
                        } else {
                            cx.notify();
                        }
                    });
                })
                .into_any_element()
            };
            let mut head = Vec::new();
            if !choice.acp() {
                head.push(row(
                    0,
                    crate::tr!("control.agents.use_every_model").into_owned(),
                    Some(crate::tr!("control.agents.use_every_model_description").into_owned()),
                    true,
                    true,
                    choice.whole(),
                ));
            }
            if !left.is_empty() {
                let all = added
                    .iter()
                    .cloned()
                    .chain(left.iter().map(|(id, _)| id.clone()))
                    .collect();
                head.push(row(
                    1,
                    crate::tr!("control.agents.add_all", count = left.len().to_string())
                        .into_owned(),
                    None,
                    false,
                    false,
                    choice.only(all),
                ));
            }
            let matching: Vec<AnyElement> = left
                .iter()
                .filter(|(_, name)| query.is_empty() || name.to_lowercase().contains(&query))
                .enumerate()
                .map(|(index, (id, name))| {
                    let mut ids = added.clone();
                    ids.push(id.clone());
                    row(
                        index + 2,
                        name.clone(),
                        (!choice.acp()).then(|| id.clone()),
                        false,
                        false,
                        choice.only(ids),
                    )
                })
                .collect();
            let none = matching.is_empty().then(|| {
                crate::material::empty_row(
                    if left.is_empty() {
                        crate::tr!("control.agents.all_added")
                    } else {
                        crate::tr!("control.agents.no_match")
                    },
                    cx,
                )
            });
            v_flex()
                .w(crate::sizing::fit_viewport(
                    280.,
                    window.viewport_size().width,
                ))
                .children(search.as_ref().map(|search| {
                    div()
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            Input::new(search)
                                .placeholder(crate::tr!("control.agents.search"))
                                .appearance(false),
                        )
                }))
                .child(v_flex().p_1().gap_0p5().children(head))
                .child(crate::material::faded_hairline(cx))
                .child(
                    v_flex()
                        .id("control-agent-add-list")
                        .max_h(crate::sizing::fit_viewport(
                            260.,
                            window.viewport_size().height,
                        ))
                        .overflow_y_scroll()
                        .p_1()
                        .gap_0p5()
                        .children(matching)
                        .children(none),
                )
                .into_any_element()
        })
        .into_any_element()
    }

    /// How `start_thread` with `subagent: true` starts a subagent.
    fn subagent_group(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let subagents = self.store.read(cx).settings().subagents;
        let options = [
            (
                ChildApprovalMode::Orchestrator,
                "child-approval-orchestrator",
                "control.subagents.child_approval.orchestrator",
                "control.subagents.child_approval.orchestrator_description",
            ),
            (
                ChildApprovalMode::Auto,
                "child-approval-auto",
                "control.subagents.child_approval.auto",
                "control.subagents.child_approval.auto_description",
            ),
            (
                ChildApprovalMode::AlwaysAllow,
                "child-approval-always-allow",
                "control.subagents.child_approval.always_allow",
                "control.subagents.child_approval.always_allow_description",
            ),
            (
                ChildApprovalMode::Manual,
                "child-approval-manual",
                "control.subagents.child_approval.manual",
                "control.subagents.child_approval.manual_description",
            ),
        ]
        .into_iter()
        .map(|(mode, id, label, description)| {
            let page = cx.entity();
            RadioOption::new(id, crate::tr!(label))
                .description(crate::tr!(description))
                .when(mode == ChildApprovalMode::default(), |option| {
                    option.badge(crate::material::chip(
                        crate::tr!("control.preset.default_badge"),
                        cx,
                    ))
                })
                .selected(mode == subagents.child_approval)
                .on_select(move |_, cx| {
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(move |store| store.set_child_approval(mode), cx)
                    })
                })
        })
        .collect::<Vec<_>>();
        let worktrees_reset = crate::settings_rows::reset_action(
            "reset-child-worktrees",
            subagents.child_worktrees,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_child_worktrees(false), cx)
            }),
        );
        let worktrees = self.toggle_row(
            "child-worktrees",
            crate::tr!("control.subagents.child_worktrees.title"),
            crate::tr!("control.subagents.child_worktrees.description"),
            subagents.child_worktrees,
            worktrees_reset,
            cx,
            WorkspaceStore::set_child_worktrees,
        );
        v_flex()
            .child(crate::material::group_caption(
                crate::tr!("control.subagents.section"),
                cx,
            ))
            .child(
                crate::material::group(cx)
                    .child(crate::settings_rows::row_frame(compact, cx).child(
                        crate::settings_rows::row_labels(
                            crate::tr!("control.subagents.child_approval.title"),
                            None,
                            None,
                            compact,
                            cx,
                        ),
                    ))
                    .child(
                        RadioList::new("child-approval")
                            .compact(compact)
                            .options(options),
                    )
                    .child(crate::material::row_hairline(
                        px(crate::material::CARD_INSET),
                        cx,
                    ))
                    .child(worktrees),
            )
            .child(crate::settings_rows::footnote(
                crate::tr!("control.subagents.footnote"),
                compact,
                cx,
            ))
            .into_any_element()
    }

    /// The activity list, by day, cross-fading between loading, failed,
    /// empty and the entries as a query answers.
    fn control_activity_group(
        &self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = match &self.control.activity {
            None => ActivityState::Loading,
            Some(Err(error)) => ActivityState::Failed(error.clone()),
            Some(Ok(page)) if !page.entries.is_empty() => ActivityState::Entries,
            Some(Ok(page)) if page.threads.is_empty() => ActivityState::Empty,
            Some(Ok(_)) => ActivityState::NoMatch,
        };
        let refresh = Button::new("control-activity-refresh")
            .debug_selector(|| "control-activity-refresh".into())
            .ghost()
            .xsmall()
            .icon(IconName::RefreshCw)
            .label(crate::tr!("control.settings.activity_refresh"))
            .disabled(self.control.activity_task.is_some())
            .on_click(cx.listener(|this, _, _, cx| this.refresh_control_activity(cx)));
        let body = crate::motion::cross_fade(
            "control-activity",
            state,
            CHANGE,
            window,
            cx,
            |state, window, cx| {
                v_flex()
                    .w_full()
                    .children(self.control_activity_state(state.clone(), compact, window, cx))
                    .into_any_element()
            },
        );
        v_flex()
            .child(
                gpui_base::h_flex()
                    .w_full()
                    .items_end()
                    .child(
                        crate::material::group_caption(crate::tr!("control.settings.activity"), cx)
                            .flex_1(),
                    )
                    .child(div().pb_1().child(refresh)),
            )
            .children(self.control_activity_filters(compact, cx))
            .child(body)
            .children(self.control_activity_more(cx))
            .child(crate::settings_rows::footnote(
                crate::tr!("control.settings.activity_footnote"),
                compact,
                cx,
            ))
            .into_any_element()
    }

    /// The thread and kind selects, once the log has anything to choose from.
    fn control_activity_filters(
        &self,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some(Ok(page)) = &self.control.activity else {
            return None;
        };
        if page.threads.is_empty() {
            return None;
        }
        let filter = &self.control.activity_filter;
        let select = |key: &'static str, value, label: String, selected: bool| SelectRowOption {
            value,
            id: SharedString::from(format!("control-activity-{key}-{label}")),
            label: label.into(),
            description: None,
            selected,
        };
        let mut threads = vec![select(
            "thread",
            None,
            crate::tr!("control.settings.activity_all_threads").into_owned(),
            filter.thread_id.is_none(),
        )];
        threads.extend(page.threads.iter().map(|thread| {
            let title = if thread.title.is_empty() {
                crate::tr!("control.settings.deleted_thread").into_owned()
            } else {
                thread.title.clone()
            };
            let selected = filter.thread_id.as_ref() == Some(&thread.id);
            select("thread", Some(thread.id.clone()), title, selected)
        }));
        let mut kinds = vec![select(
            "kind",
            None,
            crate::tr!("control.settings.activity_all_kinds").into_owned(),
            filter.action.is_none(),
        )];
        kinds.extend(page.actions.iter().map(|action| {
            let selected = filter.action.as_ref() == Some(action);
            select(
                "kind",
                Some(action.clone()),
                crate::control::action_label(action),
                selected,
            )
        }));
        let selected = |options: &[SelectRowOption<Option<String>>]| -> SharedString {
            options
                .iter()
                .find(|option| option.selected)
                .or(options.first())
                .map(|option| option.label.clone())
                .unwrap_or_default()
        };
        let page = cx.entity();
        let thread_row = crate::settings_rows::select_row(
            "control-activity-thread",
            "control-activity-thread-popover",
            "control-activity-thread-menu",
            240.,
            crate::tr!("control.settings.activity_thread")
                .into_owned()
                .into(),
            None,
            selected(&threads),
            threads,
            None,
            crate::settings_rows::flip_row(&self.motion, "control-activity-thread", compact),
            {
                let page = page.clone();
                move |thread, _, cx| {
                    page.update(cx, |page, cx| {
                        page.filter_control_activity(|filter| filter.thread_id = thread, cx)
                    })
                }
            },
            cx,
        );
        let kind_row = crate::settings_rows::select_row(
            "control-activity-kind",
            "control-activity-kind-popover",
            "control-activity-kind-menu",
            240.,
            crate::tr!("control.settings.activity_kind")
                .into_owned()
                .into(),
            None,
            selected(&kinds),
            kinds,
            None,
            crate::settings_rows::flip_row(&self.motion, "control-activity-kind", compact),
            move |action, _, cx| {
                page.update(cx, |page, cx| {
                    page.filter_control_activity(|filter| filter.action = action, cx)
                })
            },
            cx,
        );
        Some(
            crate::material::group(cx)
                .mb_3()
                .child(thread_row)
                .child(crate::material::row_hairline(
                    px(crate::material::CARD_INSET),
                    cx,
                ))
                .child(kind_row)
                .into_any_element(),
        )
    }

    /// "Show more", while the filter matches more than the list holds.
    fn control_activity_more(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Ok(page)) = &self.control.activity else {
            return None;
        };
        (page.total as usize > page.entries.len()).then(|| {
            div()
                .pt_2()
                .flex()
                .justify_center()
                .child(
                    Button::new("control-activity-more")
                        .debug_selector(|| "control-activity-more".into())
                        .ghost()
                        .small()
                        .label(crate::tr!("control.settings.activity_more"))
                        .disabled(self.control.activity_task.is_some())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.control.activity_limit += ACTIVITY_PAGE;
                            this.refresh_control_activity(cx);
                        })),
                )
                .into_any_element()
        })
    }

    fn control_activity_state(
        &self,
        state: ActivityState,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let entries = match state.clone() {
            ActivityState::Loading => {
                return vec![
                    crate::material::group(cx)
                        .child(crate::material::empty_row(
                            crate::tr!("control.settings.activity_loading"),
                            cx,
                        ))
                        .into_any_element(),
                ];
            }
            ActivityState::Failed(error) => {
                let row = crate::settings_rows::flip_row(
                    &self.motion,
                    "control-activity-failed",
                    compact,
                );
                let layout = row.row(
                    div().flex_1().text_secondary(compact, cx).child(crate::tr!(
                        "control.settings.activity_failed",
                        error = error
                    )),
                    Button::new("control-activity-retry")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("control.settings.activity_retry"))
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_control_activity(cx))),
                    cx,
                );
                return vec![
                    crate::material::group(cx)
                        .child(row.render(layout))
                        .into_any_element(),
                ];
            }
            // Entries leaving for another state draw none: they are gone.
            ActivityState::Entries => match &self.control.activity {
                Some(Ok(page)) if !page.entries.is_empty() => page.entries.clone(),
                _ => return Vec::new(),
            },
            ActivityState::Empty | ActivityState::NoMatch => {
                let message = if state == ActivityState::Empty {
                    crate::tr!("control.settings.activity_empty")
                } else {
                    crate::tr!("control.settings.activity_no_match")
                };
                return vec![
                    crate::material::group(cx)
                        .child(crate::material::empty_row(message, cx))
                        .into_any_element(),
                ];
            }
        };
        let now =
            chrono::DateTime::from_timestamp_millis(self.store.read(cx).host_now_millis(cx) as i64)
                .unwrap_or_default()
                .with_timezone(&chrono::Local);
        let mut days: Vec<(ActivityDay, Vec<ControlActivityEntry>)> = Vec::new();
        for entry in entries {
            let day = ActivityDay::of(entry.at, &now);
            match days.last_mut() {
                Some((last, list)) if *last == day => list.push(entry),
                _ => days.push((day, vec![entry])),
            }
        }
        let held: std::collections::HashSet<String> = self
            .store
            .read(cx)
            .flat_sessions()
            .into_iter()
            .map(|meta| meta.id)
            .collect();
        let now_secs = self.store.read(cx).host_now_secs(cx);
        let mut groups = Vec::new();
        for (day, entries) in days {
            // A refresh keeps the entries it found before where they were and
            // folds new ones in. Two entries alike in every field are told
            // apart by their order.
            let mut seen = std::collections::HashMap::<String, usize>::new();
            let entries: Vec<(SharedString, ControlActivityEntry)> = entries
                .into_iter()
                .map(|entry| {
                    let identity = format!(
                        "{}-{}-{}-{}-{}",
                        entry.at,
                        entry.target_thread_id,
                        entry.action.actor_thread_id,
                        entry.action.action,
                        entry.action.detail.as_deref().unwrap_or_default()
                    );
                    let count = seen.entry(identity.clone()).or_default();
                    *count += 1;
                    (SharedString::from(format!("{identity}-{count}")), entry)
                })
                .collect();
            let list = crate::settings_rows::grouped_keyed(
                SharedString::from(format!("control-activity-{day:?}")),
                entries,
                window,
                cx,
                |key, entry, _, cx| {
                    activity_row(
                        key,
                        entry,
                        held.contains(&entry.target_thread_id),
                        now_secs,
                        compact,
                        cx,
                    )
                },
            );
            groups.push(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .pl(px(crate::material::CARD_INSET))
                            .text_secondary(compact, cx)
                            .child(crate::widgets::fade_text(day.label())),
                    )
                    .child(crate::material::group(cx).child(list))
                    .into_any_element(),
            );
        }
        vec![v_flex().gap_3().children(groups).into_any_element()]
    }
}

/// "Queued a message · Delegate: migrate auth", by whom, and when; a row
/// whose target is still here opens it.
fn activity_row(
    key: &SharedString,
    entry: &ControlActivityEntry,
    target_held: bool,
    now_secs: u64,
    compact: bool,
    cx: &App,
) -> AnyElement {
    let action = crate::control::action_label(&entry.action.action);
    let own = entry.action.actor_thread_id == entry.target_thread_id;
    let target = if entry.target_title.is_empty() {
        crate::tr!("control.settings.deleted_thread").into_owned()
    } else {
        entry.target_title.clone()
    };
    let mut first = gpui_base::h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .text_body(compact, cx)
        .child(div().flex_none().child(crate::widgets::fade_text(action)));
    if !own {
        first = first.child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .font_medium()
                .child(crate::widgets::fade_text(target)),
        );
    } else {
        first = first.child(div().flex_1());
    }
    first = first.child(div().flex_none().text_secondary(compact, cx).child(
        crate::widgets::fade_text(humanize_ago(now_secs.saturating_sub(entry.at / 1000))),
    ));
    let by = if own {
        crate::tr!(
            "control.settings.activity_self",
            actor = entry.action.actor_title.as_str()
        )
    } else {
        crate::tr!(
            "control.settings.activity_by",
            actor = entry.action.actor_title.as_str()
        )
    }
    .into_owned();
    let detail = entry
        .action
        .detail
        .as_deref()
        .filter(|detail| !detail.is_empty())
        .filter(|_| crate::control::action_detail_readable(&entry.action.action));
    let second = gpui_base::h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .text_secondary(compact, cx)
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .child(crate::widgets::fade_text(match detail {
                    Some(detail) => format!("{by} · {detail}"),
                    None => by,
                })),
        );
    let body = v_flex()
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(first)
        .child(second);
    let selector = format!("control-activity-row-{key}");
    if target_held {
        let thread_id = entry.target_thread_id.clone();
        crate::material::accessible_clickable(
            crate::material::group_row(cx),
            SharedString::from(format!("control-activity-open-{key}")),
            Role::Button,
            SharedString::from(entry.target_title.clone()),
            cx,
        )
        .debug_selector(move || selector.clone())
        .cursor_pointer()
        .relative()
        .child(crate::material::row_hover(px(0.), cx))
        .child(body)
        .on_click(move |_, window, cx| open_thread(thread_id.clone(), window, cx))
        .into_any_element()
    } else {
        crate::material::group_row(cx)
            .debug_selector(move || selector.clone())
            .child(body)
            .into_any_element()
    }
}
