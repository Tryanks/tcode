use super::*;
use crate::control::{ActivityDay, GrantPreset};
use crate::scroll::ScrollableElement as _;
use crate::widgets::choice::{Choice, ChoiceGroup};
use crate::widgets::radio::{RadioList, RadioOption};
use gpui_base::StyledExt as _;
use tcode_core::control::{ControlGrant, ControlGrantRequest, ControlScope, ControlSettings};
use tcode_core::project::Project;
use tcode_core::settings::ChildApprovalMode;
use tcode_protocol::{ControlActivityEntry, ControlActivityFilter, ControlActivityPage};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

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
}

impl Default for ControlSection {
    fn default() -> Self {
        Self {
            activity: None,
            activity_task: None,
            activity_requested: false,
            activity_filter: ControlActivityFilter::default(),
            activity_limit: ACTIVITY_PAGE,
        }
    }
}

impl ControlSection {
    pub(super) fn hidden(&mut self) {
        self.activity_requested = false;
    }
}

/// The grant a thread would hold without its own: its project's exception,
/// else the default.
fn baseline_grant(control: &ControlSettings, meta: &SessionMeta) -> ControlGrant {
    control.resolve(None, meta.project_id.as_deref()).0
}

fn project_name<'a>(projects: &'a [Project], meta: &SessionMeta) -> Option<&'a str> {
    let id = meta.project_id.as_deref()?;
    projects
        .iter()
        .find(|project| project.id == id)
        .map(|project| project.name.as_str())
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

/// A thread's title as a link that opens it.
fn thread_link(id: SharedString, title: String, thread_id: String, cx: &App) -> AnyElement {
    let accent = cx.theme().link;
    crate::material::accessible_clickable(div(), id, Role::Link, title.clone(), cx)
        .min_w_0()
        .overflow_hidden()
        .cursor_pointer()
        .hover(move |link| link.text_color(accent))
        .child(crate::widgets::fade_text(title))
        .on_click(move |_, window, cx| open_thread(thread_id.clone(), window, cx))
        .into_any_element()
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
        let projects = self.store.read(cx).projects();
        let sessions = self.store.read(cx).flat_sessions();

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

        let pending = crate::control::pending_requests(sessions.iter().cloned());
        let extra: Vec<SessionMeta> = sessions
            .iter()
            .filter(|meta| meta.control_grant.is_some())
            .cloned()
            .collect();
        let quiet = pending.is_empty() && extra.is_empty();

        // Everything under the switch stays as it is while the switch is off,
        // dimmed: turning it back on restores it.
        let dim = gpui_base::motion::transition(
            "control-dim",
            if control.enabled { 1. } else { 0.5 },
            gpui_base::motion::Transition::new(CHANGE),
            window,
            cx,
        );
        let mut below = v_flex().gap(crate::material::section_gap(cx));
        let pending_group =
            self.control_pending_group(pending, &control, &projects, compact, window, cx);
        below = below.children(pending_group);
        below = below.child(self.control_default_group(&control, quiet, compact, cx));
        below = below.child(self.control_project_group(&control, &projects, compact, window, cx));
        below = below
            .children(self.control_thread_group(extra, &control, &projects, compact, window, cx));
        below = below.child(self.subagent_group(compact, cx));
        below = below.child(self.control_activity_group(compact, window, cx));

        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::material::group(cx).child(enabled))
            .child(below.opacity(dim))
    }

    /// The grant requests every thread is waiting on, answered here as in the
    /// thread; the group is drawn only while there is one.
    fn control_pending_group(
        &mut self,
        pending: Vec<(SessionMeta, ControlGrantRequest)>,
        control: &ControlSettings,
        projects: &[Project],
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let count = pending.len();
        let items: Vec<(SharedString, (SessionMeta, ControlGrantRequest))> = pending
            .into_iter()
            .map(|(meta, request)| {
                (
                    SharedString::from(format!("{}-{}", meta.id, request.id)),
                    (meta, request),
                )
            })
            .collect();
        let shown = crate::motion::shown(
            "control-pending",
            (count > 0).then_some(()),
            SHOW,
            window,
            cx,
        )?;
        let caption = crate::tr!("control.settings.pending", count = count).into_owned();
        let list = crate::settings_rows::grouped_keyed(
            "control-pending-list",
            items,
            window,
            cx,
            |_, (meta, request), _, cx| {
                self.control_pending_row(meta, request, control, projects, compact, cx)
            },
        );
        Some(
            shown.fold(
                v_flex()
                    .child(crate::material::group_caption(caption, cx))
                    .child(crate::material::group(cx).child(list))
                    .child(crate::settings_rows::footnote(
                        crate::tr!("control.settings.pending_footnote"),
                        compact,
                        cx,
                    )),
            ),
        )
    }

    fn control_pending_row(
        &self,
        meta: &SessionMeta,
        request: &ControlGrantRequest,
        control: &ControlSettings,
        projects: &[Project],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let own_project = project_name(projects, meta);
        let wants =
            crate::control::grant_phrase(&request.grant, own_project, projects).unwrap_or_default();
        let current = control
            .resolve(meta.control_grant.as_ref(), meta.project_id.as_deref())
            .0;
        let now = crate::control::current_grant_sentence(&current, own_project, projects);
        let mut context: Vec<String> = own_project.map(str::to_owned).into_iter().collect();
        context.push(self.store.read(cx).harness_name_of(meta));
        context.push(humanize_ago(
            self.store
                .read(cx)
                .host_now_secs(cx)
                .saturating_sub(request.requested_at),
        ));
        let respond = |approve: bool| {
            let (session_id, request_id) = (meta.id.clone(), request.id.clone());
            cx.listener(move |this: &mut Self, _, _, cx| {
                let (session_id, request_id) = (session_id.clone(), request_id.clone());
                this.store.update(cx, |store, _| {
                    store.respond_control_grant(session_id, request_id, approve)
                });
            })
        };
        let key = format!("{}-{}", meta.id, request.id);
        let heading = if compact {
            v_flex()
                .w_full()
                .min_w_0()
                .child(thread_link(
                    SharedString::from(format!("control-pending-thread-{key}")),
                    meta.title.clone(),
                    meta.id.clone(),
                    cx,
                ))
                .child(div().text_secondary(compact, cx).child(context.join(" · ")))
        } else {
            gpui_base::h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .child(thread_link(
                    SharedString::from(format!("control-pending-thread-{key}")),
                    meta.title.clone(),
                    meta.id.clone(),
                    cx,
                ))
                .child(
                    div()
                        .flex_none()
                        .text_secondary(compact, cx)
                        .child(context.join(" · ")),
                )
        };
        crate::material::group_row_stacked(cx)
            .debug_selector(move || format!("control-pending-{key}"))
            .child(div().text_body(compact, cx).child(heading))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_body(compact, cx)
                            .font_medium()
                            .child(crate::tr!("control.request.wants", what = wants)),
                    )
                    .child(div().text_secondary(compact, cx).child(now)),
            )
            .when(!request.reason.trim().is_empty(), |row| {
                row.child(div().text_secondary(compact, cx).italic().child(crate::tr!(
                    "control.request.reason",
                    reason = request.reason.trim()
                )))
            })
            .child(
                gpui_base::h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!(
                            "control-pending-approve-{}",
                            request.id
                        )))
                        .debug_selector({
                            let selector = format!("control-pending-approve-{}", request.id);
                            move || selector.clone()
                        })
                        .primary()
                        .small()
                        .label(crate::tr!("control.request.approve"))
                        .on_click(respond(true)),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "control-pending-decline-{}",
                            request.id
                        )))
                        .debug_selector({
                            let selector = format!("control-pending-decline-{}", request.id);
                            move || selector.clone()
                        })
                        .danger()
                        .outline()
                        .small()
                        .label(crate::tr!("control.request.decline"))
                        .on_click(respond(false)),
                    ),
            )
            .into_any_element()
    }

    /// The default grant as the preset it is, or custom; its scope beside.
    fn control_default_group(
        &mut self,
        control: &ControlSettings,
        quiet: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let default = control.default_grant.clone();
        let current = GrantPreset::of(&default);
        let set_default = |grant: ControlGrant, page: &Entity<Self>, cx: &mut App| {
            page.update(cx, |page, cx| {
                page.dispatch_settings(move |store| store.set_control_default_grant(grant), cx)
            })
        };
        let mut options: Vec<RadioOption> = GrantPreset::ALL
            .into_iter()
            .map(|preset| {
                let page = cx.entity();
                let next = preset.apply(&default);
                RadioOption::new(
                    SharedString::from(format!("control-preset-{}", preset.name())),
                    preset.label(),
                )
                .description(preset.description())
                .when(preset == GrantPreset::Standard, |option| {
                    option.badge(crate::material::chip(
                        crate::tr!("control.preset.default_badge"),
                        cx,
                    ))
                })
                .selected(current == Some(preset))
                .on_select(move |_, cx| set_default(next.clone(), &page, cx))
            })
            .collect();
        let custom_description = if current.is_none() && !default.categories.is_empty() {
            crate::tr!(
                "control.preset.custom_enabled",
                items = default
                    .categories
                    .iter()
                    .map(|category| crate::control::category_label(*category))
                    .collect::<Vec<_>>()
                    .join(&crate::tr!("list.separator"))
            )
            .into_owned()
        } else {
            crate::tr!("control.preset.custom_description").into_owned()
        };
        let store = self.store.clone();
        options.push(
            RadioOption::new("control-preset-custom", crate::tr!("control.preset.custom"))
                .description(custom_description)
                .selected(current.is_none())
                .on_select(move |window, cx| {
                    open_grant_editor(store.clone(), GrantTarget::Default, window, cx)
                }),
        );
        let presets = RadioList::new("control-default-presets")
            .compact(compact)
            .options(options);

        let scope_choices = [
            (ControlScope::Project, crate::tr!("control.scope.project")),
            (ControlScope::Host, crate::tr!("control.scope.host")),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (scope, label))| {
            let page = cx.entity();
            let mut next = default.clone();
            next.scope = scope.clone();
            Choice::new(
                SharedString::from(format!("control-default-scope-{index}")),
                label,
            )
            .selected(default.scope == scope)
            .on_select(move |_, cx| set_default(next.clone(), &page, cx))
        });
        let scope_note = matches!(default.scope, ControlScope::Projects(_)).then(|| {
            SharedString::from(crate::tr!("control.scope.selected_in_custom").into_owned())
        });
        let row = self.flip_row("control-default-scope", cx);
        let scope = row.row(
            crate::settings_rows::row_labels(
                crate::tr!("control.scope.title"),
                scope_note,
                None,
                compact,
                cx,
            ),
            ChoiceGroup::new("control-default-scope")
                .compact(compact)
                .aria_label(crate::tr!("control.scope.title"))
                .choices(scope_choices),
            cx,
        );
        v_flex()
            .child(crate::material::group_caption(
                crate::tr!("control.settings.default_grant"),
                cx,
            ))
            .child(
                crate::material::group(cx)
                    .child(presets)
                    .child(crate::material::row_hairline(
                        px(crate::material::CARD_INSET),
                        cx,
                    ))
                    .child(row.render(scope)),
            )
            .when(quiet, |group| {
                group.child(crate::settings_rows::footnote(
                    crate::tr!("control.settings.default_footnote_quiet"),
                    compact,
                    cx,
                ))
            })
            .into_any_element()
    }

    /// The projects that have an exception, each edited in the grant editor,
    /// and the row that sets one for another project.
    fn control_project_group(
        &mut self,
        control: &ControlSettings,
        projects: &[Project],
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let overrides: Vec<(SharedString, (Project, String))> = projects
            .iter()
            .filter_map(|project| {
                let summary = control
                    .project_overrides
                    .get(&project.id)
                    .map(crate::control::grant_summary)?;
                Some((
                    SharedString::from(project.id.clone()),
                    (project.clone(), summary),
                ))
            })
            .collect();
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
                v_flex()
                    .w_full()
                    .child(
                        crate::material::group_row(cx)
                            .debug_selector({
                                let id = id.clone();
                                move || format!("control-override-{id}")
                            })
                            .text_body(compact, cx)
                            .child(crate::project_icon::artwork(
                                self.store.read(cx).image_host(),
                                project,
                                16.,
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .overflow_hidden()
                                            .child(crate::widgets::fade_text(project.name.clone())),
                                    )
                                    .when(compact, |labels| {
                                        labels.child(
                                            div()
                                                .text_secondary(compact, cx)
                                                .child(summary.clone()),
                                        )
                                    }),
                            )
                            .when(!compact, |row| {
                                row.child(div().text_secondary(compact, cx).child(summary.clone()))
                            })
                            .child(
                                Button::new(SharedString::from(format!("edit-grant-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .label(crate::tr!("control.settings.edit"))
                                    .on_click(move |_, window, cx| {
                                        open_grant_editor(
                                            store.clone(),
                                            GrantTarget::Project(edit.clone()),
                                            window,
                                            cx,
                                        )
                                    }),
                            )
                            .child(
                                Button::new(SharedString::from(format!("remove-grant-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip(crate::tr!("control.settings.remove_override"))
                                    .aria_label(crate::tr!("control.settings.remove_override"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let id = id.clone();
                                        this.dispatch_settings(
                                            move |store| store.set_control_project_grant(id, None),
                                            cx,
                                        )
                                    })),
                            ),
                    )
                    .child(crate::material::row_hairline(
                        px(crate::material::CARD_INSET),
                        cx,
                    ))
                    .into_any_element()
            },
        );
        let options: Vec<SelectRowOption<Project>> = projects
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
        let add = crate::settings_rows::select_row(
            "add-project-grant",
            "project-grant-popover",
            "project-grant-menu",
            240.,
            crate::tr!("control.settings.add_override")
                .into_owned()
                .into(),
            None,
            crate::tr!("control.settings.choose_project")
                .into_owned()
                .into(),
            options,
            None,
            crate::settings_rows::flip_row(&self.motion, "add-project-grant", compact),
            {
                let page = cx.entity();
                move |project: Project, window, cx| {
                    let grant = default_grant.clone();
                    let id = project.id.clone();
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(
                            move |store| store.set_control_project_grant(id, Some(grant)),
                            cx,
                        )
                    });
                    open_grant_editor(store.clone(), GrantTarget::Project(project), window, cx);
                }
            },
            cx,
        );
        v_flex()
            .child(crate::material::group_caption(
                crate::tr!("control.settings.overrides"),
                cx,
            ))
            .child(
                crate::material::group(cx)
                    .child(list.render(v_flex().w_full()))
                    .child(add),
            )
            .into_any_element()
    }

    /// Threads holding a grant of their own, each by how it differs from the
    /// one it would otherwise have; drawn only while there is one.
    fn control_thread_group(
        &mut self,
        threads: Vec<SessionMeta>,
        control: &ControlSettings,
        projects: &[Project],
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let count = threads.len();
        let shown = crate::motion::shown(
            "control-threads",
            (count > 0).then_some(()),
            SHOW,
            window,
            cx,
        )?;
        let items: Vec<(SharedString, SessionMeta)> = threads
            .into_iter()
            .map(|meta| (SharedString::from(meta.id.clone()), meta))
            .collect();
        let list = crate::settings_rows::grouped_keyed(
            "control-thread-list",
            items,
            window,
            cx,
            |_, meta, _, cx| self.control_thread_row(meta, control, projects, compact, cx),
        );
        Some(
            shown.fold(
                v_flex()
                    .child(crate::material::group_caption(
                        crate::tr!("control.settings.threads", count = count),
                        cx,
                    ))
                    .child(crate::material::group(cx).child(list))
                    .child(crate::settings_rows::footnote(
                        crate::tr!("control.settings.threads_footnote"),
                        compact,
                        cx,
                    )),
            ),
        )
    }

    fn control_thread_row(
        &self,
        meta: &SessionMeta,
        control: &ControlSettings,
        projects: &[Project],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let grant = meta.control_grant.clone().unwrap_or_default();
        let deltas = crate::control::grant_deltas(&grant, &baseline_grant(control, meta));
        let theme = cx.theme();
        let chips: Vec<AnyElement> = if deltas.is_empty() {
            vec![
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("control.settings.same_as_default"))
                    .into_any_element(),
            ]
        } else {
            deltas
                .iter()
                .map(|delta| {
                    let label = crate::control::delta_label(delta, projects);
                    if delta.sensitive() {
                        crate::material::semantic_chip(
                            label,
                            theme.warning.opacity(0.12),
                            theme.warning_foreground,
                            cx,
                        )
                    } else {
                        crate::material::chip(label, cx)
                    }
                    .into_any_element()
                })
                .collect()
        };
        let id = meta.id.clone();
        let revoke = Button::new(SharedString::from(format!(
            "revoke-thread-grant-{}",
            meta.id
        )))
        .debug_selector({
            let selector = format!("revoke-thread-grant-{}", meta.id);
            move || selector.clone()
        })
        .ghost()
        .when(!compact, |button| button.xsmall())
        .when(compact, |button| button.small())
        .label(crate::tr!("control.settings.revoke"))
        .on_click(cx.listener(move |this, _, _, cx| {
            let id = id.clone();
            this.store
                .update(cx, |store, _| store.clear_control_grant(id));
        }));
        let harness = match &meta.acp_agent_id {
            Some(id) => crate::harness_dialog::HarnessId::Acp(id.clone()),
            None => crate::harness_dialog::HarnessId::Native(meta.provider),
        };
        let no_mcp = crate::harness_dialog::describe(self.store.read(cx), &harness)
            .is_some_and(|description| !description.mcp_servers);
        let mut title_line = gpui_base::h_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .text_body(compact, cx)
            .child(thread_link(
                SharedString::from(format!("control-thread-link-{}", meta.id)),
                meta.title.clone(),
                meta.id.clone(),
                cx,
            ));
        if let Some(name) = project_name(projects, meta) {
            title_line = title_line.child(
                div()
                    .flex_none()
                    .text_secondary(compact, cx)
                    .child(format!("· {name}")),
            );
        }
        let labels = v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(title_line)
            .child(gpui_base::h_flex().flex_wrap().gap_1().children(chips))
            .when(no_mcp, |labels| {
                labels.child(
                    div()
                        .text_secondary(compact, cx)
                        .child(crate::tr!("control.settings.no_mcp")),
                )
            });
        let selector = format!("control-thread-{}", meta.id);
        if compact {
            let thread_id = meta.id.clone();
            crate::material::group_row_stacked(cx)
                .debug_selector(move || selector.clone())
                .child(labels)
                .child(
                    gpui_base::h_flex().gap_2().child(revoke).child(
                        Button::new(SharedString::from(format!("open-thread-grant-{}", meta.id)))
                            .ghost()
                            .small()
                            .label(crate::tr!("control.settings.open"))
                            .on_click(move |_, window, cx| {
                                open_thread(thread_id.clone(), window, cx)
                            }),
                    ),
                )
                .into_any_element()
        } else {
            crate::material::group_row(cx)
                .debug_selector(move || selector.clone())
                .child(labels)
                .child(revoke)
                .into_any_element()
        }
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
                            .child(day.label()),
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
    let theme = cx.theme();
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
        .child(div().flex_none().child(action));
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
    first = first.child(
        div()
            .flex_none()
            .text_secondary(compact, cx)
            .child(humanize_ago(now_secs.saturating_sub(entry.at / 1000))),
    );
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
    let mut second = gpui_base::h_flex()
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
    if let Some(category) = crate::control::action_category(&entry.action.action) {
        second = second.child(crate::material::semantic_chip(
            crate::control::category_label(category),
            theme.warning.opacity(0.12),
            theme.warning_foreground,
            cx,
        ));
    }
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

/// Which grant the editor changes.
#[derive(Clone)]
pub(crate) enum GrantTarget {
    Default,
    Project(Project),
}

/// The grant editor in a dialog; every edit is saved as it is made.
pub(crate) fn open_grant_editor(
    store: Entity<WorkspaceStore>,
    target: GrantTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let title: SharedString = match &target {
        GrantTarget::Default => crate::tr!("control.settings.default_dialog_title"),
        GrantTarget::Project(project) => crate::tr!(
            "control.settings.dialog_title",
            project = project.name.clone()
        ),
    }
    .into_owned()
    .into();
    let project = matches!(target, GrantTarget::Project(_));
    let editor = cx.new(|cx| GrantEditor::new(store, target, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        let content = editor.clone();
        let reset = editor.clone();
        let mut actions = crate::overlay::DialogActions::new();
        if project {
            actions = actions.child(
                Button::new("project-grant-default")
                    .label(crate::tr!("control.settings.remove_override"))
                    .on_click(move |_, window, cx| {
                        reset.update(cx, |editor, cx| editor.use_default(cx));
                        window.close_dialog(cx);
                    }),
            );
        }
        dialog
            .sheet()
            .title(title.clone())
            .content(move |body, _, _| body.child(content.clone()))
            .footer(
                actions.child(
                    Button::new("project-grant-done")
                        .primary()
                        .label(crate::tr!("control.settings.done"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    });
}

struct GrantEditor {
    store: Entity<WorkspaceStore>,
    target: GrantTarget,
    _subscription: Subscription,
}

impl GrantEditor {
    fn new(store: Entity<WorkspaceStore>, target: GrantTarget, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            target,
            _subscription: subscription,
        }
    }

    fn use_default(&mut self, cx: &mut Context<Self>) {
        let GrantTarget::Project(project) = &self.target else {
            return;
        };
        let id = project.id.clone();
        self.store.update(cx, |store, cx| {
            store.set_control_project_grant(id, None);
            store.local_settings_changed(cx);
        });
    }
}

impl Render for GrantEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let control = store.settings().control;
        let projects = store.projects();
        let grant = match &self.target {
            GrantTarget::Default => control.default_grant.clone(),
            GrantTarget::Project(project) => control
                .project_overrides
                .get(&project.id)
                .cloned()
                .unwrap_or(control.default_grant),
        };
        let compact = crate::window_seam::window_is_compact(window, cx);
        let store = self.store.clone();
        let target = self.target.clone();
        let editor = crate::control::grant_editor(
            "grant-dialog",
            &grant,
            &projects,
            compact,
            Rc::new(move |grant, _, cx| {
                let target = target.clone();
                store.update(cx, |store, cx| {
                    match target {
                        GrantTarget::Default => store.set_control_default_grant(grant),
                        GrantTarget::Project(project) => {
                            store.set_control_project_grant(project.id, Some(grant))
                        }
                    }
                    store.local_settings_changed(cx);
                })
            }),
            cx,
        );
        // The window turning compact steps the editor's type and choices up
        // to touch sizes; the dialog's height glides to the new layout's.
        let height = crate::motion::GlideState::keyed("grant-dialog-height", window, cx);
        div()
            .id("grant-dialog-body")
            .size_full()
            .overflow_y_scroll_area()
            .child(crate::motion::SizeGlide::new(&height, editor).on_change(u64::from(compact)))
    }
}
