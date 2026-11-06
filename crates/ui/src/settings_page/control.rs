use super::*;
use tcode_core::control::{ControlGrant, SettlementAutomation};
use tcode_core::project::Project;
use tcode_core::settings::ChildApprovalMode;
use tcode_protocol::ControlActivityEntry;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// How many recent `tcode_control` writes the section lists.
const ACTIVITY_LIMIT: u32 = 50;

/// What the activity list shows; the entries themselves are read where drawn.
#[derive(Clone, PartialEq)]
enum ActivityState {
    Loading,
    Failed(String),
    Empty,
    Entries,
}

#[derive(Default)]
pub(super) struct ControlSection {
    activity: Option<Result<Vec<ControlActivityEntry>, String>>,
    activity_task: Option<gpui::Task<()>>,
    /// Latches the one activity query fired when the section opens.
    activity_requested: bool,
    /// The project whose grant the compact page shows open.
    expanded_project: Option<String>,
}

impl ControlSection {
    pub(super) fn hidden(&mut self) {
        self.activity_requested = false;
    }
}

impl SettingsPage {
    fn refresh_control_activity(&mut self, cx: &mut Context<Self>) {
        self.control.activity_requested = true;
        let query = self
            .store
            .update(cx, |store, cx| store.control_activity(ACTIVITY_LIMIT, cx));
        self.control.activity_task = Some(cx.spawn(async move |this, cx| {
            let result = query.await;
            let _ = this.update(cx, |this, cx| {
                this.control.activity = Some(result);
                this.control.activity_task = None;
                cx.notify();
            });
        }));
    }

    fn project_grant_change(
        &self,
        project_id: String,
        cx: &Context<Self>,
    ) -> crate::control::GrantChange {
        let page = cx.entity();
        Rc::new(move |grant, _, cx| {
            let project_id = project_id.clone();
            page.update(cx, |page, cx| {
                page.dispatch_settings(
                    move |store| store.set_control_project_grant(project_id, Some(grant)),
                    cx,
                )
            })
        })
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
        let projects = self.store.read(cx).projects();

        let enabled_reset = crate::settings_rows::reset_action(
            "reset-control-enabled",
            !control.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_control_enabled(true), cx)
            }),
        );
        let automation = settings.settlement_automation;
        let automation_reset = crate::settings_rows::reset_action(
            "reset-settlement-automation",
            !automation.is_default(),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_settlement_automation(SettlementAutomation::default()),
                    cx,
                )
            }),
        );
        let automation_label = |value: SettlementAutomation| -> SharedString {
            match value {
                SettlementAutomation::Off => crate::tr!("control.settings.automation_off"),
                SettlementAutomation::Suggest => crate::tr!("control.settings.automation_suggest"),
                SettlementAutomation::ExplicitSignals => {
                    crate::tr!("control.settings.automation_explicit")
                }
            }
            .into_owned()
            .into()
        };
        let automation_options = [
            (
                SettlementAutomation::Off,
                "settlement-automation-off",
                "control.settings.automation_off_description",
            ),
            (
                SettlementAutomation::Suggest,
                "settlement-automation-suggest",
                "control.settings.automation_suggest_description",
            ),
            (
                SettlementAutomation::ExplicitSignals,
                "settlement-automation-explicit",
                "control.settings.automation_explicit_description",
            ),
        ]
        .into_iter()
        .map(|(value, id, description)| SelectRowOption {
            value,
            id: id.into(),
            label: automation_label(value),
            description: Some(crate::tr!(description).into_owned().into()),
            selected: value == automation,
        })
        .collect();
        let general = vec![
            self.toggle_row(
                "control-enabled",
                crate::tr!("control.settings.enabled"),
                crate::tr!("control.settings.enabled_description"),
                control.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_control_enabled,
            ),
            crate::settings_rows::select_row(
                "settlement-automation-dropdown",
                "settlement-automation-popover",
                "settlement-automation-menu",
                320.,
                crate::tr!("control.settings.automation")
                    .into_owned()
                    .into(),
                crate::tr!("control.settings.automation_description")
                    .into_owned()
                    .into(),
                automation_label(automation),
                automation_options,
                automation_reset,
                self.flip_row("settlement-automation-dropdown", cx),
                {
                    let page = cx.entity();
                    move |value, _, cx| {
                        page.update(cx, |page, cx| {
                            page.dispatch_settings(
                                move |store| store.set_settlement_automation(value),
                                cx,
                            )
                        })
                    }
                },
                cx,
            ),
        ];

        let default_reset = crate::settings_rows::reset_action(
            "reset-control-default-grant",
            control.default_grant != ControlGrant::default_grant(),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_control_default_grant(ControlGrant::default_grant()),
                    cx,
                )
            }),
        );
        let page = cx.entity();
        let default_editor = crate::control::grant_editor(
            "control-default-grant",
            &control.default_grant,
            &projects,
            compact,
            Rc::new(move |grant, _, cx| {
                page.update(cx, |page, cx| {
                    page.dispatch_settings(move |store| store.set_control_default_grant(grant), cx)
                })
            }),
            cx,
        );

        let subagent_rows = self.subagent_rows(cx);
        let project_rows = self.control_project_rows(&control, projects, compact, window, cx);
        let thread_rows = self.control_thread_rows(compact, window, cx);
        let activity_rows = self.control_activity_rows(compact, window, cx);
        let heading = self.flip_row("control-default-grant", cx);
        let heading_row = heading.frame(cx).child(heading.part(
            "labels",
            crate::settings_rows::row_labels(
                crate::control::grant_summary(&control.default_grant),
                Some(crate::tr!("control.settings.default_grant_description").into()),
                default_reset,
                compact,
                cx,
            ),
        ));
        let default_heading = vec![heading.render(heading_row)];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.title"),
                general,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.subagents.section"),
                subagent_rows,
                cx,
            ))
            .child(
                crate::settings_rows::settings_group(
                    crate::tr!("control.settings.default_grant"),
                    default_heading,
                    cx,
                )
                .gap_2()
                .child(default_editor),
            )
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.settings.overrides"),
                project_rows,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.settings.threads"),
                thread_rows,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("control.settings.activity"),
                activity_rows,
                cx,
            ))
    }

    /// How `start_thread` with `subagent: true` starts a subagent.
    fn subagent_rows(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let subagents = self.store.read(cx).settings().subagents;
        let approval_reset = crate::settings_rows::reset_action(
            "reset-child-approval",
            subagents.child_approval != ChildApprovalMode::default(),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_child_approval(ChildApprovalMode::default()),
                    cx,
                )
            }),
        );
        let approval_label = |mode: ChildApprovalMode| -> SharedString {
            match mode {
                ChildApprovalMode::Orchestrator => {
                    crate::tr!("control.subagents.child_approval.orchestrator")
                }
                ChildApprovalMode::Auto => crate::tr!("control.subagents.child_approval.auto"),
                ChildApprovalMode::AlwaysAllow => {
                    crate::tr!("control.subagents.child_approval.always_allow")
                }
                ChildApprovalMode::Manual => crate::tr!("control.subagents.child_approval.manual"),
            }
            .into_owned()
            .into()
        };
        let approval_options = [
            (
                ChildApprovalMode::Orchestrator,
                "child-approval-orchestrator",
                "control.subagents.child_approval.orchestrator_description",
            ),
            (
                ChildApprovalMode::Auto,
                "child-approval-auto",
                "control.subagents.child_approval.auto_description",
            ),
            (
                ChildApprovalMode::AlwaysAllow,
                "child-approval-always-allow",
                "control.subagents.child_approval.always_allow_description",
            ),
            (
                ChildApprovalMode::Manual,
                "child-approval-manual",
                "control.subagents.child_approval.manual_description",
            ),
        ]
        .into_iter()
        .map(|(mode, id, description)| SelectRowOption {
            value: mode,
            id: id.into(),
            label: approval_label(mode),
            description: Some(crate::tr!(description).into_owned().into()),
            selected: mode == subagents.child_approval,
        })
        .collect();
        let worktrees_reset = crate::settings_rows::reset_action(
            "reset-child-worktrees",
            subagents.child_worktrees,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_child_worktrees(false), cx)
            }),
        );
        vec![
            crate::settings_rows::select_row(
                "child-approval-dropdown",
                "child-approval-popover",
                "child-approval-menu",
                340.,
                crate::tr!("control.subagents.child_approval.title")
                    .into_owned()
                    .into(),
                crate::tr!("control.subagents.child_approval.description")
                    .into_owned()
                    .into(),
                approval_label(subagents.child_approval),
                approval_options,
                approval_reset,
                self.flip_row("child-approval-dropdown", cx),
                {
                    let page = cx.entity();
                    move |mode, _, cx| {
                        page.update(cx, |page, cx| {
                            page.dispatch_settings(move |store| store.set_child_approval(mode), cx)
                        })
                    }
                },
                cx,
            ),
            self.toggle_row(
                "child-worktrees",
                crate::tr!("control.subagents.child_worktrees.title"),
                crate::tr!("control.subagents.child_worktrees.description"),
                subagents.child_worktrees,
                worktrees_reset,
                cx,
                WorkspaceStore::set_child_worktrees,
            ),
        ]
    }

    fn control_project_rows(
        &mut self,
        control: &tcode_core::control::ControlSettings,
        projects: Vec<Project>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // The phone's accordion and the desktop's dropdown and list trade
        // places when the window turns, cross-fading while the group's
        // height glides.
        let swap = self.motion.swap(
            "control-projects-form",
            usize::from(compact),
            2,
            CHANGE,
            window,
            cx,
        );
        let mut layers = Vec::new();
        for (layer, compact) in [(0, false), (1, true)] {
            if swap.shows(layer) {
                let rows = if compact {
                    self.control_project_accordion(control, &projects, window, cx)
                } else {
                    self.control_project_dropdown(control, &projects, window, cx)
                };
                layers.push((layer, v_flex().w_full().children(rows).into_any_element()));
            }
        }
        vec![swap.render(layers)]
    }

    /// Phone: every project is a row that opens its grant in place; there
    /// is no dropdown or dialog to pick one from.
    fn control_project_accordion(
        &mut self,
        control: &tcode_core::control::ControlSettings,
        projects: &[Project],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        {
            rows.push(
                crate::settings_rows::row_frame(true, cx)
                    .child(crate::settings_rows::row_labels(
                        crate::tr!("control.settings.overrides"),
                        Some(crate::tr!("control.settings.overrides_description").into()),
                        None,
                        true,
                        cx,
                    ))
                    .into_any_element(),
            );
            let list = projects
                .iter()
                .map(|project| (SharedString::from(project.id.clone()), project.clone()));
            let list = crate::motion::keyed(
                "control-projects",
                list,
                ROWS,
                window,
                cx,
                |_, project, window, cx| {
                    let own = control.project_overrides.get(&project.id);
                    let open =
                        self.control.expanded_project.as_deref() == Some(project.id.as_str());
                    let toggle = project.id.clone();
                    let summary = crate::motion::shown(
                        SharedString::from(format!("control-project-summary-{}", project.id)),
                        own.map(crate::control::grant_summary),
                        SHOW,
                        window,
                        cx,
                    );
                    let row = crate::material::accessible_clickable(
                        crate::material::group_row(cx),
                        SharedString::from(format!("control-project-{}", project.id)),
                        Role::Button,
                        SharedString::from(project.name.clone()),
                        cx,
                    )
                    .aria_expanded(open)
                    .cursor_pointer()
                    .relative()
                    .child(crate::material::row_hover(gpui::px(0.), cx))
                    .text_body(true, cx)
                    .child(crate::project_icon::artwork(project, 16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(project.name.clone())),
                    )
                    .children(summary.map(|summary| {
                        summary.fade(div().text_secondary(true, cx).child(summary.value.clone()))
                    }))
                    .child(
                        crate::widgets::Chevron::new(
                            SharedString::from(format!("control-project-chevron-{}", project.id)),
                            open,
                        )
                        .text_color(cx.theme().muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.control.expanded_project =
                            if this.control.expanded_project.as_ref() == Some(&toggle) {
                                None
                            } else {
                                Some(toggle.clone())
                            };
                        cx.notify();
                    }));
                    // The open project's grant folds out under its row; the one
                    // it replaces folds away.
                    let fold_id =
                        SharedString::from(format!("control-project-fold-{}", project.id));
                    let shown =
                        crate::motion::shown(fold_id.clone(), open.then_some(()), SHOW, window, cx);
                    let mut fold = crate::widgets::Collapsible::new(fold_id, open)
                        .motion(&self.motion)
                        .w_full();
                    if shown.is_some() {
                        let grant = own
                            .cloned()
                            .unwrap_or_else(|| control.default_grant.clone());
                        let id = project.id.clone();
                        let remove = crate::motion::shown(
                            SharedString::from(format!("control-project-default-{}", project.id)),
                            own.is_some().then_some(()),
                            SHOW,
                            window,
                            cx,
                        );
                        fold = fold.content(
                            crate::material::group_row_stacked(cx)
                                .child(crate::control::grant_editor(
                                    SharedString::from(format!(
                                        "control-project-grant-{}",
                                        project.id
                                    )),
                                    &grant,
                                    projects,
                                    true,
                                    self.project_grant_change(project.id.clone(), cx),
                                    cx,
                                ))
                                .when_some(remove, |row, remove| {
                                    row.child(
                                        remove.fade(
                                            Button::new(SharedString::from(format!(
                                                "control-project-default-{}",
                                                project.id
                                            )))
                                            .ghost()
                                            .label(crate::tr!("control.settings.remove_override"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                let id = id.clone();
                                                this.dispatch_settings(
                                                    move |store| {
                                                        store.set_control_project_grant(id, None)
                                                    },
                                                    cx,
                                                )
                                            })),
                                        ),
                                    )
                                }),
                        );
                    }
                    v_flex().w_full().child(row).child(fold).into_any_element()
                },
            );
            rows.push(list.render(v_flex().w_full()));
        }
        rows
    }

    /// Desktop: a dropdown adds a project's grant and the list below edits
    /// them in a dialog.
    fn control_project_dropdown(
        &mut self,
        control: &tcode_core::control::ControlSettings,
        projects: &[Project],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        let options = projects
            .iter()
            .filter(|project| !control.project_overrides.contains_key(&project.id))
            .map(|project| SelectRowOption {
                id: SharedString::from(format!("control-add-{}", project.id)),
                label: project.name.clone().into(),
                value: project.clone(),
                selected: false,
                description: None,
            })
            .collect();
        let default_grant = control.default_grant.clone();
        let store = self.store.clone();
        rows.push(crate::settings_rows::select_row(
            "add-project-grant",
            "project-grant-popover",
            "project-grant-menu",
            240.,
            crate::tr!("control.settings.overrides").into_owned().into(),
            crate::tr!("control.settings.overrides_description")
                .into_owned()
                .into(),
            crate::tr!("control.settings.add_override")
                .into_owned()
                .into(),
            options,
            None,
            crate::settings_rows::flip_row(&self.motion, "add-project-grant", false),
            {
                let page = cx.entity();
                move |project, window, cx| {
                    let grant = default_grant.clone();
                    let id = project.id.clone();
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(
                            move |store| store.set_control_project_grant(id, Some(grant)),
                            cx,
                        )
                    });
                    open_project_grant(store.clone(), project, window, cx);
                }
            },
            cx,
        ));
        let overrides = projects.iter().filter_map(|project| {
            let summary = control
                .project_overrides
                .get(&project.id)
                .map(crate::control::grant_summary)?;
            Some((
                SharedString::from(project.id.clone()),
                (project.clone(), summary),
            ))
        });
        let list = crate::motion::keyed(
            "control-overrides",
            overrides,
            ROWS,
            window,
            cx,
            |_, (project, summary), _, cx| {
                let edit = project.clone();
                let id = project.id.clone();
                let store = self.store.clone();
                crate::material::group_row(cx)
                    .text_body(false, cx)
                    .child(crate::project_icon::artwork(project, 16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(project.name.clone())),
                    )
                    .child(div().text_secondary(false, cx).child(summary.clone()))
                    .child(
                        Button::new(SharedString::from(format!("edit-grant-{id}")))
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("control.settings.edit"))
                            .on_click(move |_, window, cx| {
                                open_project_grant(store.clone(), edit.clone(), window, cx)
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("remove-grant-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(crate::tr!("control.settings.remove_override"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = id.clone();
                                this.dispatch_settings(
                                    move |store| store.set_control_project_grant(id, None),
                                    cx,
                                )
                            })),
                    )
                    .into_any_element()
            },
        );
        rows.push(list.render(v_flex().w_full()));
        rows
    }

    /// Threads holding a grant: a revoked one folds away, and the empty row
    /// is one more row, which leaves as the first is granted and comes back
    /// once the last is revoked.
    fn control_thread_rows(
        &self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let threads: Vec<(SharedString, Option<SessionMeta>)> = self
            .store
            .read(cx)
            .flat_sessions()
            .into_iter()
            .filter(|meta| meta.control_grant.is_some())
            .map(|meta| (SharedString::from(meta.id.clone()), Some(meta)))
            .collect();
        let threads = if threads.is_empty() {
            vec![("control-threads-empty".into(), None)]
        } else {
            threads
        };
        let list = crate::motion::keyed(
            "control-threads",
            threads,
            ROWS,
            window,
            cx,
            |_, meta, _, cx| match meta {
                Some(meta) => self.control_thread_row(meta, compact, cx),
                None => {
                    crate::material::empty_row(crate::tr!("control.settings.threads_empty"), cx)
                        .into_any_element()
                }
            },
        );
        vec![list.render(v_flex().w_full())]
    }

    fn control_thread_row(
        &self,
        meta: &SessionMeta,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = meta
            .control_grant
            .as_ref()
            .map(crate::control::grant_summary)
            .unwrap_or_default();
        let id = meta.id.clone();
        let row = crate::settings_rows::flip_row(
            &self.motion,
            format!("control-thread-{}", meta.id),
            compact,
        );
        let layout = row.row(
            crate::settings_rows::row_labels(
                meta.title.clone(),
                Some(summary.into()),
                None,
                compact,
                cx,
            ),
            Button::new(SharedString::from(format!(
                "revoke-thread-grant-{}",
                meta.id
            )))
            .ghost()
            .when(!compact, |button| button.xsmall())
            .label(crate::tr!("control.settings.revoke"))
            .on_click(cx.listener(move |this, _, _, cx| {
                let id = id.clone();
                this.store
                    .update(cx, |store, _| store.clear_control_grant(id));
            })),
            cx,
        );
        row.render(layout)
    }

    /// The activity list, cross-fading between loading, failed, empty and
    /// the entries as a query answers.
    fn control_activity_rows(
        &self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let state = match &self.control.activity {
            None => ActivityState::Loading,
            Some(Err(error)) => ActivityState::Failed(error.clone()),
            Some(Ok(entries)) if entries.is_empty() => ActivityState::Empty,
            Some(Ok(_)) => ActivityState::Entries,
        };
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
        vec![body]
    }

    fn control_activity_state(
        &self,
        state: ActivityState,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let refresh = Button::new("control-activity-refresh")
            .ghost()
            .xsmall()
            .icon(IconName::RefreshCw)
            .tooltip(crate::tr!("control.settings.activity_refresh"))
            .aria_label(crate::tr!("control.settings.activity_refresh"))
            .disabled(self.control.activity_task.is_some())
            .on_click(cx.listener(|this, _, _, cx| this.refresh_control_activity(cx)));
        let entries = match state {
            ActivityState::Loading => {
                return vec![
                    crate::material::empty_row(crate::tr!("control.settings.activity_loading"), cx)
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
                    div().flex_1().text_secondary(compact, cx).child(error),
                    refresh,
                    cx,
                );
                return vec![row.render(layout)];
            }
            // Entries leaving for another state draw none: they are gone.
            ActivityState::Entries => match &self.control.activity {
                Some(Ok(entries)) if !entries.is_empty() => entries,
                _ => return Vec::new(),
            },
            ActivityState::Empty => {
                return vec![
                    crate::material::group_row(cx)
                        .child(
                            div()
                                .flex_1()
                                .text_secondary(compact, cx)
                                .child(crate::tr!("control.settings.activity_empty")),
                        )
                        .child(refresh)
                        .into_any_element(),
                ];
            }
        };
        let now = now_secs();
        let mut rows = vec![
            crate::material::group_row(cx)
                .child(div().flex_1())
                .child(refresh)
                .into_any_element(),
        ];
        // A refresh keeps the entries it found before where they were and
        // folds new ones in. Two entries alike in every field are told apart
        // by their order.
        let mut seen = std::collections::HashMap::<String, usize>::new();
        let entries = entries.iter().map(|entry| {
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
            (
                SharedString::from(format!("{identity}-{count}")),
                entry.clone(),
            )
        });
        let entries = crate::motion::keyed(
            "control-activity-entries",
            entries.collect::<Vec<_>>(),
            ROWS,
            window,
            cx,
            |key, entry, _, cx| {
                let target = if entry.target_title.is_empty() {
                    crate::tr!("control.settings.deleted_thread").into_owned()
                } else {
                    entry.target_title.clone()
                };
                let title = crate::tr!(
                    "control.settings.activity_row",
                    actor = entry.action.actor_title.as_str(),
                    target = target
                )
                .into_owned();
                let mut action = crate::control::action_label(&entry.action.action);
                if let Some(detail) = entry.action.detail.as_deref().filter(|d| !d.is_empty()) {
                    action.push_str(&format!(" ({detail})"));
                }
                let row = crate::settings_rows::flip_row(
                    &self.motion,
                    format!("control-activity-{key}"),
                    compact,
                );
                let layout = row.row(
                    crate::settings_rows::row_labels(title, Some(action.into()), None, compact, cx),
                    div()
                        .flex_none()
                        .text_secondary(compact, cx)
                        .child(humanize_ago(now.saturating_sub(entry.at / 1000))),
                    cx,
                );
                row.render(layout)
            },
        );
        rows.push(entries.render(v_flex().w_full()));
        rows
    }
}

/// A project's grant in a dialog; every edit is saved as it is made.
pub(crate) fn open_project_grant(
    store: Entity<WorkspaceStore>,
    project: Project,
    window: &mut Window,
    cx: &mut App,
) {
    let editor = cx.new(|cx| ProjectGrantEditor::new(store, project, cx));
    let title: SharedString = crate::tr!(
        "control.settings.dialog_title",
        project = editor.read(cx).project.name.clone()
    )
    .into_owned()
    .into();
    window.open_dialog(cx, move |dialog, _, _| {
        let content = editor.clone();
        let reset = editor.clone();
        dialog
            .title(title.clone())
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                crate::overlay::DialogActions::new()
                    .child(
                        Button::new("project-grant-default")
                            .label(crate::tr!("control.settings.remove_override"))
                            .on_click(move |_, window, cx| {
                                reset.update(cx, |editor, cx| editor.use_default(cx));
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("project-grant-done")
                            .primary()
                            .label(crate::tr!("control.settings.done"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    });
}

struct ProjectGrantEditor {
    store: Entity<WorkspaceStore>,
    project: Project,
    _subscription: Subscription,
}

impl ProjectGrantEditor {
    fn new(store: Entity<WorkspaceStore>, project: Project, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            project,
            _subscription: subscription,
        }
    }

    fn use_default(&mut self, cx: &mut Context<Self>) {
        let id = self.project.id.clone();
        self.store.update(cx, |store, cx| {
            store.set_control_project_grant(id, None);
            store.local_settings_changed(cx);
        });
    }
}

impl Render for ProjectGrantEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let control = store.settings().control;
        let projects = store.projects();
        let grant = control
            .project_overrides
            .get(&self.project.id)
            .cloned()
            .unwrap_or(control.default_grant);
        let compact = crate::window_seam::window_is_compact(window, cx);
        let store = self.store.clone();
        let project_id = self.project.id.clone();
        let editor = crate::control::grant_editor(
            "project-grant-dialog",
            &grant,
            &projects,
            compact,
            Rc::new(move |grant, _, cx| {
                let project_id = project_id.clone();
                store.update(cx, |store, cx| {
                    store.set_control_project_grant(project_id, Some(grant));
                    store.local_settings_changed(cx);
                })
            }),
            cx,
        );
        // The window turning compact steps the editor's type and choices up
        // to touch sizes; the dialog's height glides to the new layout's.
        let height = crate::motion::GlideState::keyed("project-grant-dialog-height", window, cx);
        crate::motion::SizeGlide::new(&height, editor).on_change(u64::from(compact))
    }
}
