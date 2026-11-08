use super::*;
use tcode_core::{
    control::SettlementAutomation,
    project::Project,
    settings::{ProjectSettlementSettings, Settings},
};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) fn open_project_rules(
    store: Entity<WorkspaceStore>,
    project: Project,
    window: &mut Window,
    cx: &mut App,
) {
    let editor = cx.new(|cx| ProjectRulesEditor::new(store, project, window, cx));
    let title = editor.read(cx).title();
    window.open_dialog(cx, move |dialog, _, _| {
        let content = editor.clone();
        let save = editor.clone();
        let button_save = editor.clone();
        dialog
            .title(title.clone())
            .content(move |body, _, _| body.child(content.clone()))
            .on_ok(move |_, window, cx| save.update(cx, |editor, cx| editor.save(window, cx)))
            .footer(
                crate::overlay::DialogActions::new()
                    .child(
                        Button::new("project-rules-cancel")
                            .label(crate::tr!("settings.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("project-rules-save")
                            .primary()
                            .label(crate::tr!("settings.auto_settle.save"))
                            .on_click(move |_, window, cx| {
                                if button_save.update(cx, |editor, cx| editor.save(window, cx)) {
                                    window.close_dialog(cx);
                                }
                            }),
                    ),
            )
    });
}

impl SettingsPage {
    pub(super) fn commit_auto_settle_days(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stored = self
            .store
            .read(cx)
            .settings()
            .auto_settle_after_days
            .unwrap_or(3.);
        let parsed = self
            .auto_settle_input
            .state
            .read(cx)
            .value()
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(1., 90.));
        let value = parsed.unwrap_or(stored);
        self.auto_settle_input.dirty = false;
        self.auto_settle_input.push(value.to_string(), window, cx);
        if parsed.is_some() {
            self.dispatch_settings(
                move |store| store.set_auto_settle_after_days(Some(value)),
                cx,
            );
        }
    }

    pub(super) fn render_threads(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let delete_confirm_reset = crate::settings_rows::reset_action(
            "reset-delete-confirm",
            settings.skip_delete_confirmation,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_skip_delete_confirmation(false), cx)
            }),
        );
        let scroll_on_send_reset = crate::settings_rows::reset_action(
            "reset-scroll-to-latest-on-send",
            !settings.scroll_to_latest_on_send,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_scroll_to_latest_on_send(true), cx)
            }),
        );
        let resume_on_limit_reset_reset = crate::settings_rows::reset_action(
            "reset-resume-on-limit-reset",
            !settings.resume_on_limit_reset,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_resume_on_limit_reset(true), cx)
            }),
        );
        let mut threads = vec![
            self.title_generation_row(cx),
            self.toggle_row(
                "delete-confirm",
                crate::tr!("settings.delete_confirmation.title"),
                crate::tr!("settings.delete_confirmation.description"),
                !settings.skip_delete_confirmation,
                delete_confirm_reset,
                cx,
                |store, checked| store.set_skip_delete_confirmation(!checked),
            ),
            self.toggle_row(
                "scroll-to-latest-on-send",
                crate::tr!("settings.scroll_to_latest_on_send.title"),
                crate::tr!("settings.scroll_to_latest_on_send.description"),
                settings.scroll_to_latest_on_send,
                scroll_on_send_reset,
                cx,
                WorkspaceStore::set_scroll_to_latest_on_send,
            ),
            self.toggle_row(
                "resume-on-limit-reset",
                crate::tr!("settings.resume_on_limit_reset.title"),
                crate::tr!("settings.resume_on_limit_reset.description"),
                settings.resume_on_limit_reset,
                resume_on_limit_reset_reset,
                cx,
                WorkspaceStore::set_resume_on_limit_reset,
            ),
        ];
        if self.settings_hosts(cx).len() > 1 {
            threads.push(self.provider_marks_row(cx));
        }
        let settling = self.settling_rows(&settings, window, cx);
        let panels = self.panel_rows(&settings, cx);
        let can_manage_host = self.store.read(cx).can_manage_host();
        if can_manage_host {
            self.fetch_host_home(cx);
        }
        let projects = vec![self.project_start_dir_row(cx)];
        let compact = self.window_state.read(cx).compact;
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.threads_section"),
                threads,
                cx,
            ))
            .child(
                crate::settings_rows::settings_group(
                    crate::tr!("settings.settling_section"),
                    settling,
                    cx,
                )
                .child(crate::settings_rows::footnote(
                    crate::tr!("settings.auto_settle.footnote"),
                    compact,
                    cx,
                )),
            )
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.panels_section"),
                panels,
                cx,
            ))
            .when(can_manage_host, |page| {
                page.child(crate::settings_rows::settings_group(
                    crate::tr!("settings.projects_section"),
                    projects,
                    cx,
                ))
            })
    }

    fn panel_rows(&mut self, settings: &Settings, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let live_command_panel_reset = crate::settings_rows::reset_action(
            "reset-live-command-panel",
            settings.live_command_panel_disabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_live_command_panel_disabled(false), cx)
            }),
        );
        let word_wrap_reset = crate::settings_rows::reset_action(
            "reset-word-wrap",
            settings.word_wrap_diffs,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_word_wrap_diffs(false), cx)
            }),
        );
        vec![
            self.toggle_row(
                "live-command-panel",
                crate::tr!("settings.live_command_panel.title"),
                crate::tr!("settings.live_command_panel.description"),
                !settings.live_command_panel_disabled,
                live_command_panel_reset,
                cx,
                |store, checked| store.set_live_command_panel_disabled(!checked),
            ),
            self.toggle_row(
                "word-wrap",
                crate::tr!("settings.word_wrap.title"),
                crate::tr!("settings.word_wrap.description"),
                settings.word_wrap_diffs,
                word_wrap_reset,
                cx,
                WorkspaceStore::set_word_wrap_diffs,
            ),
        ]
    }

    /// Auto-settle, its days and merge rules, and the per-project overrides:
    /// a phone lists every project as a link to its rules page; a wide window
    /// adds overrides from a menu and edits them in a dialog.
    /// How far Tcode settles threads on its own.
    fn settlement_automation_row(
        &mut self,
        settings: &Settings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let automation = settings.settlement_automation;
        let reset = crate::settings_rows::reset_action(
            "reset-settlement-automation",
            !automation.is_default(),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_settlement_automation(SettlementAutomation::default()),
                    cx,
                )
            }),
        );
        let label = |value: SettlementAutomation| -> SharedString {
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
        let options = [
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
            label: label(value),
            description: Some(crate::tr!(description).into_owned().into()),
            selected: value == automation,
        })
        .collect();
        crate::settings_rows::select_row(
            "settlement-automation-dropdown",
            "settlement-automation-popover",
            "settlement-automation-menu",
            320.,
            crate::tr!("control.settings.automation")
                .into_owned()
                .into(),
            Some(
                crate::tr!("control.settings.automation_description")
                    .into_owned()
                    .into(),
            ),
            label(automation),
            options,
            reset,
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
        )
    }

    fn settling_rows(
        &mut self,
        settings: &Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let reset = crate::settings_rows::reset_action(
            "reset-auto-settle",
            settings.auto_settle_after_days != Some(3.),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_auto_settle_after_days(Some(3.)), cx)
            }),
        );
        let mut rows = vec![self.settlement_automation_row(settings, cx)];
        rows.push(self.toggle_row(
            "auto-settle-inactive",
            crate::tr!("settings.auto_settle.title"),
            crate::tr!("settings.auto_settle.description"),
            settings.auto_settle_after_days.is_some(),
            reset,
            cx,
            |store, checked| store.set_auto_settle_after_days(checked.then_some(3.)),
        ));
        // The days apply only while auto-settle is on, and fold with it.
        rows.push(
            crate::widgets::Collapsible::new(
                "auto-settle-days-fold",
                settings.auto_settle_after_days.is_some(),
            )
            .motion(&self.motion)
            .w_full()
            .content({
                let row = self.flip_row("auto-settle-days", cx);
                let layout = row.row(
                    crate::settings_rows::row_labels(
                        crate::tr!("settings.auto_settle.days"),
                        Some(crate::tr!("settings.auto_settle.days_description").into()),
                        None,
                        row.compact(),
                        cx,
                    ),
                    crate::widgets::input::NumberInput::new(&self.auto_settle_input.state)
                        .accessibility_label(crate::tr!("settings.auto_settle.days")),
                    cx,
                );
                row.render(layout)
            })
            .into_any_element(),
        );
        let merge_reset = crate::settings_rows::reset_action(
            "reset-auto-settle-on-merge",
            settings.auto_settle_on_merge,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_auto_settle_on_merge(false), cx)
            }),
        );
        rows.push(self.toggle_row(
            "auto-settle-on-merge",
            crate::tr!("settings.auto_settle.on_merge"),
            crate::tr!("settings.auto_settle.on_merge_description"),
            settings.auto_settle_on_merge,
            merge_reset,
            cx,
            |store, checked| store.set_auto_settle_on_merge(checked),
        ));
        let projects = self.store.read(cx).projects();
        let summary = |project: &Project| -> Option<SharedString> {
            let rules = settings.project_settlement_overrides.get(&project.id)?;
            let parts: Vec<SharedString> = rules
                .auto_settle_after_days
                .map(days_label)
                .into_iter()
                .chain(rules.auto_settle_on_merge.map(|on| {
                    crate::tr!(
                        "settings.auto_settle.summary_merge",
                        value = on_off_label(on).to_string()
                    )
                    .into_owned()
                    .into()
                }))
                .collect();
            (!parts.is_empty()).then(|| parts.join(" · ").into())
        };
        // The phone's project links and the desktop's dropdown and list
        // trade places when the window turns, cross-fading while the
        // group's height glides.
        let compact = self.window_state.read(cx).compact;
        let swap = self.motion.swap(
            "project-rules-form",
            usize::from(compact),
            2,
            CHANGE,
            window,
            cx,
        );
        let mut layers = Vec::new();
        for (layer, compact) in [(0, false), (1, true)] {
            if !swap.shows(layer) {
                continue;
            }
            let mut form = Vec::new();
            if compact {
                // Phone: every project is a row that pushes its rules page; there
                // is no dropdown or dialog to pick one from.
                form.push(
                    crate::settings_rows::row_frame(true, cx)
                        .child(crate::settings_rows::row_labels(
                            crate::tr!("settings.auto_settle.overrides"),
                            Some(crate::tr!("settings.auto_settle.overrides_description").into()),
                            None,
                            true,
                            cx,
                        ))
                        .into_any_element(),
                );
                let projects = projects
                    .clone()
                    .into_iter()
                    .map(|project| (SharedString::from(project.id.clone()), project));
                let list = crate::motion::keyed(
                    "project-rules",
                    projects,
                    ROWS,
                    window,
                    cx,
                    |_, project, _, cx| {
                        let summary = summary(project);
                        let edit = project.clone();
                        crate::material::accessible_clickable(
                            crate::material::group_row(cx),
                            SharedString::from(format!("project-rules-{}", project.id)),
                            Role::Button,
                            SharedString::from(project.name.clone()),
                            cx,
                        )
                        .cursor_pointer()
                        .relative()
                        .child(crate::material::row_hover(gpui::px(0.), cx))
                        .text_body(true, cx)
                        .child(crate::project_icon::artwork(
                            self.store.read(cx).image_host(),
                            project,
                            16.,
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(project.name.clone())),
                        )
                        .children(
                            summary.map(|summary| div().text_secondary(true, cx).child(summary)),
                        )
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .text_color(cx.theme().muted_foreground),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.edit_project_rules(edit.clone(), window, cx)
                        }))
                        .into_any_element()
                    },
                );
                form.push(list.render(v_flex().w_full()));
            } else {
                let options = projects
                    .iter()
                    .filter(|project| {
                        !settings
                            .project_settlement_overrides
                            .contains_key(&project.id)
                    })
                    .map(|project| SelectRowOption {
                        id: project.id.clone().into(),
                        label: project.name.clone().into(),
                        value: project.clone(),
                        selected: false,
                        description: None,
                    })
                    .collect();
                form.push(crate::settings_rows::select_row(
                    "add-project-rules",
                    "project-rules-popover",
                    "project-rules-menu",
                    240.,
                    crate::tr!("settings.auto_settle.overrides")
                        .into_owned()
                        .into(),
                    Some(
                        crate::tr!("settings.auto_settle.overrides_description")
                            .into_owned()
                            .into(),
                    ),
                    crate::tr!("settings.auto_settle.add_override")
                        .into_owned()
                        .into(),
                    options,
                    None,
                    crate::settings_rows::flip_row(&self.motion, "add-project-rules", false),
                    {
                        let page = cx.entity();
                        move |project, window, cx| {
                            page.update(cx, |page, cx| page.edit_project_rules(project, window, cx))
                        }
                    },
                    cx,
                ));
                let overrides = projects
                    .clone()
                    .into_iter()
                    .filter(|project| {
                        settings
                            .project_settlement_overrides
                            .contains_key(&project.id)
                    })
                    .map(|project| {
                        let summary = summary(&project).unwrap_or_default();
                        (SharedString::from(project.id.clone()), (project, summary))
                    });
                let list = crate::motion::keyed(
                    "settle-overrides",
                    overrides,
                    ROWS,
                    window,
                    cx,
                    |_, (project, summary), _, cx| {
                        let edit = project.clone();
                        let id = project.id.clone();
                        crate::material::group_row(cx)
                            .text_body(false, cx)
                            .child(crate::project_icon::artwork(
                                self.store.read(cx).image_host(),
                                project,
                                16.,
                            ))
                            .child(div().flex_1().child(project.name.clone()))
                            .child(div().text_secondary(false, cx).child(summary.clone()))
                            .child(
                                Button::new(SharedString::from(format!("edit-rules-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .label(crate::tr!("settings.auto_settle.edit"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit_project_rules(edit.clone(), window, cx)
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("remove-rules-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip(crate::tr!("settings.auto_settle.remove_override"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.dispatch_settings(
                                            |store| store.set_project_settlement(id.clone(), None),
                                            cx,
                                        )
                                    })),
                            )
                            .into_any_element()
                    },
                );
                form.push(list.render(v_flex().w_full()));
            }
            layers.push((layer, v_flex().w_full().children(form).into_any_element()));
        }
        rows.push(swap.render(layers));
        rows
    }

    fn edit_project_rules(
        &mut self,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_state.read(cx).compact {
            self.project_rules_editor =
                Some(cx.new(|cx| ProjectRulesEditor::new(self.store.clone(), project, window, cx)));
            self.project_rules_subscription = self.project_rules_editor.as_ref().map(|editor| {
                cx.subscribe(editor, |this, _, _: &gpui::DismissEvent, cx| {
                    this.window_state.update(cx, |state, cx| state.back(cx));
                    this.section = Section::Threads;
                    this.project_rules_editor = None;
                    cx.notify();
                })
            });
            self.section = Section::ProjectThreadRules;
            self.window_state.update(cx, |state, cx| {
                state.go(crate::window_state::Destination::SettingsThreadRules, cx)
            });
            cx.notify();
        } else {
            open_project_rules(self.store.clone(), project, window, cx);
        }
    }
}

fn days_label(value: Option<f64>) -> SharedString {
    value.map_or_else(
        || crate::tr!("settings.auto_settle.never").into_owned().into(),
        |days| {
            if days == 1. {
                crate::tr!("settings.auto_settle.days_value_one")
            } else {
                crate::tr!("settings.auto_settle.days_value", count = days)
            }
            .into_owned()
            .into()
        },
    )
}

pub(super) struct ProjectRulesEditor {
    store: Entity<WorkspaceStore>,
    pub(super) project: Project,
    /// The editor's size-class motion: it is drawn as a phone page or in a
    /// settings section under different parents.
    motion: crate::motion::owned::Owned,
    draft: ProjectSettlementSettings,
    days: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectRulesEditor {
    fn new(
        store: Entity<WorkspaceStore>,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = store.read(cx).settings();
        let draft = settings
            .project_settlement_overrides
            .get(&project.id)
            .cloned()
            .unwrap_or_default();
        let value = draft
            .auto_settle_after_days
            .flatten()
            .or(settings.auto_settle_after_days)
            .unwrap_or(3.);
        let days = cx.new(|cx| {
            InputState::new(window, cx)
                .step(1.)
                .min(1.)
                .max(90.)
                .default_value(value.to_string())
        });
        let subscriptions = vec![
            cx.subscribe_in(&days, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    this.commit_days(window, cx);
                }
            }),
        ];
        Self {
            store,
            project,
            motion: Default::default(),
            draft,
            days,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn title(&self) -> SharedString {
        crate::tr!(
            "settings.auto_settle.dialog_title",
            project = self.project.name.clone()
        )
        .into_owned()
        .into()
    }

    fn commit_days(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let stored = self.draft.auto_settle_after_days.flatten().unwrap_or(3.);
        let parsed = self
            .days
            .read(cx)
            .value()
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| value.clamp(1., 90.));
        let value = parsed.unwrap_or(stored);
        self.days.update(cx, |input, cx| {
            input.set_value(value.to_string(), window, cx)
        });
        if matches!(self.draft.auto_settle_after_days, Some(Some(_))) {
            self.draft.auto_settle_after_days = Some(Some(value));
        }
        parsed.is_some()
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.commit_days(window, cx);
        let value =
            (self.draft != ProjectSettlementSettings::default()).then(|| self.draft.clone());
        self.store.update(cx, |store, _| {
            store.set_project_settlement(self.project.id.clone(), value)
        });
        true
    }
}

impl Render for ProjectRulesEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.store.read(cx).settings();
        let default = days_label(settings.auto_settle_after_days);
        let days_choices = vec![
            (
                None,
                crate::tr!(
                    "settings.auto_settle.use_default",
                    value = default.to_string()
                )
                .into_owned(),
            ),
            (
                Some(None),
                crate::tr!("settings.auto_settle.never").into_owned(),
            ),
            (
                Some(Some(
                    self.draft.auto_settle_after_days.flatten().unwrap_or(3.),
                )),
                crate::tr!("settings.auto_settle.after_days").into_owned(),
            ),
        ];
        let selected = self.draft.auto_settle_after_days;
        let days_index = match selected {
            None => 0,
            Some(None) => 1,
            Some(Some(_)) => 2,
        };
        let merge_choices = vec![
            (
                None,
                crate::tr!(
                    "settings.auto_settle.use_default",
                    value = on_off_label(settings.auto_settle_on_merge).to_string()
                )
                .into_owned(),
            ),
            (Some(true), on_off_label(true).into()),
            (Some(false), on_off_label(false).into()),
        ];
        let merge_index = match self.draft.auto_settle_on_merge {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        let compact = crate::window_seam::window_is_compact(window, cx);
        let mut content = v_flex()
            .gap_3()
            .child(
                div()
                    .text_body(compact, cx)
                    .child(crate::tr!("settings.auto_settle.title")),
            )
            .child(rule_selector(
                "project-auto-settle",
                days_choices,
                days_index,
                |editor, value| editor.draft.auto_settle_after_days = value,
                &self.motion,
                window,
                cx,
            ));
        content = content.child(
            crate::widgets::Collapsible::new(
                "project-auto-settle-days-fold",
                matches!(self.draft.auto_settle_after_days, Some(Some(_))),
            )
            .motion(&self.motion)
            .w_full()
            .content(
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::tr!("settings.auto_settle.days")),
                    )
                    .child(
                        crate::widgets::input::NumberInput::new(&self.days)
                            .accessibility_label(crate::tr!("settings.auto_settle.days")),
                    ),
            ),
        );
        let merge = rule_selector(
            "project-auto-settle-merge",
            merge_choices,
            merge_index,
            |editor, value| editor.draft.auto_settle_on_merge = value,
            &self.motion,
            window,
            cx,
        );
        // A phone page saves with its own button, which folds in under the
        // last choice when the window turns compact and away when it turns
        // wide, bringing its gap with it.
        let save = self.motion.swap(
            "save-project-rules",
            usize::from(compact),
            2,
            CHANGE,
            window,
            cx,
        );
        let layers = save.shows(1).then(|| {
            (
                1,
                div()
                    .pt_3()
                    .child(
                        Button::new("save-project-rules")
                            .label(crate::tr!("settings.auto_settle.save"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save(window, cx);
                                cx.emit(gpui::DismissEvent);
                            })),
                    )
                    .into_any_element(),
            )
        });
        content
            .child(
                div()
                    .text_body(compact, cx)
                    .child(crate::tr!("settings.auto_settle.on_merge")),
            )
            .child(v_flex().w_full().child(merge).child(save.render(layers)))
    }
}

fn on_off_label(on: bool) -> SharedString {
    if on {
        crate::tr!("settings.auto_settle.on")
    } else {
        crate::tr!("settings.auto_settle.off")
    }
    .into_owned()
    .into()
}

/// One project rule's choices: a phone page lists them; a dialog opens a dropdown.
fn rule_selector<T: Clone + 'static>(
    id: &'static str,
    choices: Vec<(T, String)>,
    selected: usize,
    pick: fn(&mut ProjectRulesEditor, T),
    motion: &crate::motion::owned::Owned,
    window: &mut Window,
    cx: &mut Context<ProjectRulesEditor>,
) -> AnyElement {
    let compact = crate::window_seam::window_is_compact(window, cx);
    // The list and the dropdown trade places when the window turns.
    let swap = motion.swap(id, usize::from(compact), 2, CHANGE, window, cx);
    let mut layers = Vec::new();
    if swap.shows(0) {
        layers.push((0, rule_dropdown(id, choices.clone(), selected, pick, cx)));
    }
    if swap.shows(1) {
        layers.push((1, rule_list(id, choices, selected, pick, window, cx)));
    }
    swap.render(layers)
}

fn rule_list<T: Clone + 'static>(
    id: &'static str,
    choices: Vec<(T, String)>,
    selected: usize,
    pick: fn(&mut ProjectRulesEditor, T),
    window: &mut Window,
    cx: &mut Context<ProjectRulesEditor>,
) -> AnyElement {
    let editor = cx.entity();
    {
        let rows = choices
            .into_iter()
            .enumerate()
            .map(|(index, (value, label))| {
                let checked = index == selected;
                let editor = editor.clone();
                // The mark fades from the last choice to the new one.
                let mark = gpui_base::motion::transition(
                    SharedString::from(format!("{id}-check-{index}")),
                    if checked { 1. } else { 0. },
                    gpui_base::motion::Transition::new(GLIDE),
                    window,
                    cx,
                );
                crate::material::accessible_clickable(
                    crate::material::group_row(cx),
                    (SharedString::from(format!("{id}-choice")), index),
                    Role::Button,
                    SharedString::from(label.clone()),
                    cx,
                )
                .aria_selected(checked)
                .cursor_pointer()
                .relative()
                .child(crate::material::row_hover(gpui::px(0.), cx))
                .text_body(true, cx)
                .child(div().flex_1().child(label))
                .when(mark > 0., |row| {
                    row.child(
                        Icon::new(IconName::Check)
                            .text_color(cx.theme().primary)
                            .opacity(mark),
                    )
                })
                .on_click(move |_, _, cx| {
                    editor.update(cx, |editor, cx| {
                        pick(editor, value.clone());
                        cx.notify();
                    })
                })
                .into_any_element()
            })
            .collect();
        crate::material::grouped(rows, cx).into_any_element()
    }
}

fn rule_dropdown<T: Clone + 'static>(
    id: &'static str,
    choices: Vec<(T, String)>,
    selected: usize,
    pick: fn(&mut ProjectRulesEditor, T),
    cx: &mut Context<ProjectRulesEditor>,
) -> AnyElement {
    let editor = cx.entity();
    let trigger_label = choices
        .get(selected)
        .map(|(_, label)| label.clone())
        .unwrap_or_default();
    crate::material::overlay_popover(id, cx)
        .trigger(
            Button::new(SharedString::from(format!("{id}-choice")))
                .ghost()
                .outline()
                .compact()
                .child(
                    gpui_base::h_flex()
                        .w_full()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(trigger_label)
                        .child(
                            Icon::new(IconName::ChevronDown)
                                .text_color(cx.theme().muted_foreground),
                        ),
                ),
        )
        .content(move |_, _, cx| {
            let popover = cx.entity();
            // The menu is at least as wide as its trigger; gpui-base's Select is not adopted yet.
            v_flex().p_1().min_w(px(240.)).gap_0p5().children(
                choices
                    .clone()
                    .into_iter()
                    .enumerate()
                    .map(|(index, (value, label))| {
                        let editor = editor.clone();
                        let popover = popover.clone();
                        let checked = index == selected;
                        crate::material::accessible_clickable(
                            crate::material::menu_item_frame(
                                false,
                                crate::material::ItemFill::Hover,
                                cx,
                            ),
                            (SharedString::from(format!("{id}-option")), index),
                            Role::MenuItem,
                            SharedString::from(label.clone()),
                            cx,
                        )
                        .aria_selected(checked)
                        .w_full()
                        .text_body(false, cx)
                        .cursor_pointer()
                        .child(div().flex_1().child(label))
                        .when(checked, |item| item.child(Icon::new(IconName::Check)))
                        .on_click(move |_, window, cx| {
                            editor.update(cx, |editor, cx| {
                                pick(editor, value.clone());
                                cx.notify();
                            });
                            popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                        })
                    }),
            )
        })
        .into_any_element()
}
impl gpui::EventEmitter<gpui::DismissEvent> for ProjectRulesEditor {}
