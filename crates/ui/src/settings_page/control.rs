use super::*;
use crate::control::ActivityDay;
use crate::widgets::radio::{RadioList, RadioOption};
use gpui_base::StyledExt as _;
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

        // Everything under the switch stays as it is while the switch is off,
        // dimmed: turning it back on restores it.
        let dim = gpui_base::motion::transition(
            "control-dim",
            if control.enabled { 1. } else { 0.5 },
            gpui_base::motion::Transition::new(CHANGE),
            window,
            cx,
        );
        let below = v_flex()
            .gap(crate::material::section_gap(cx))
            .child(self.subagent_group(compact, cx))
            .child(self.control_activity_group(compact, window, cx));

        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::material::group(cx).child(enabled))
            .child(below.opacity(dim))
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
