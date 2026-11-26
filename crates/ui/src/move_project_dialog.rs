//! Move to machine: hand a project and all its threads to another machine of
//! the group. The machine that holds the project asks the other one what the
//! move would take, sends everything there and lets the project go only once
//! the other machine has it; this client chooses, shows and can cancel.

use crate::overlay::{DialogActions, DialogButtons, OverlayExt as _};
use crate::scroll::AutoscrollTarget as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::checkbox::Checkbox;
use crate::widgets::input::{Input, InputState};
use crate::widgets::progress::Progress;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _,
};
use gpui_base::v_flex;
use tcode_protocol::{
    Command, GroupMachineInfo, MoveBlocker, MovePhase, ProjectMovePreview, ProjectMoveStatus,
    ProtocolError,
};

use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::thread_export::format_size;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How many of the ignored files the dialog names before it counts the rest.
const IGNORED_SHOWN: usize = 6;

/// What a machine of the group is called: the label the group gave it, else
/// its own name.
pub(crate) fn machine_title(machine: &GroupMachineInfo) -> String {
    machine.label.as_deref().unwrap_or(&machine.name).to_owned()
}

/// The machines of the group `store`'s machine is in, as that machine last
/// reported them; empty outside a group or where this client may not manage
/// the machine.
pub(crate) fn group_machines(
    store: &Entity<WorkspaceStore>,
    cx: &mut App,
) -> Vec<GroupMachineInfo> {
    crate::remote::spaces::for_store(store, cx)
        .and_then(|spaces| spaces.read(cx).group().map(|group| group.machines.clone()))
        .unwrap_or_default()
}

/// What `store`'s machine calls machine `id` of its group.
pub(crate) fn machine_name(store: &Entity<WorkspaceStore>, id: &str, cx: &mut App) -> String {
    group_machines(store, cx)
        .iter()
        .find(|machine| machine.id == id)
        .map(machine_title)
        .unwrap_or_else(|| id.to_owned())
}

/// Whether a project of `store`'s machine could be offered a move: a full
/// device of a machine whose group has another member.
pub(crate) fn offers_move(store: &Entity<WorkspaceStore>, cx: &mut App) -> bool {
    store.read(cx).scope().is_full()
        && group_machines(store, cx)
            .iter()
            .any(|machine| !machine.this_machine)
}

/// How far a move is, in a line: the row under the project and the progress
/// dialog say the same.
pub(crate) fn status_line(status: &ProjectMoveStatus, machine: &str) -> String {
    match status.phase {
        MovePhase::Preparing => crate::tr!("machines.move.status_preparing", machine = machine),
        MovePhase::SendingCode | MovePhase::SendingThreads => crate::tr!(
            "machines.move.status_sending",
            machine = machine,
            percent = percent(status).round() as u32
        ),
        MovePhase::Committing => crate::tr!("machines.move.status_finishing", machine = machine),
        MovePhase::Paused => match status.error.as_deref() {
            Some(reason) => crate::tr!("machines.move.status_paused", reason = reason),
            None => crate::tr!("machines.move.status_paused_bare"),
        },
        MovePhase::Cancelling => crate::tr!("machines.move.status_cancelling"),
    }
    .into_owned()
}

fn percent(status: &ProjectMoveStatus) -> f32 {
    if status.total == 0 {
        0.
    } else {
        status.done as f32 * 100. / status.total as f32
    }
}

/// The move's bar: a sweep while nothing can be counted, the share sent
/// otherwise, muted while the move waits.
pub(crate) fn status_bar(
    id: impl Into<gpui::ElementId>,
    status: &ProjectMoveStatus,
    cx: &App,
) -> Progress {
    let counted = matches!(
        status.phase,
        MovePhase::SendingCode | MovePhase::SendingThreads | MovePhase::Paused
    );
    Progress::new(id)
        .loading(!counted)
        .value(percent(status))
        .when(status.phase == MovePhase::Paused, |bar| {
            bar.color(cx.theme().muted_foreground)
        })
}

/// The answer to the preview asked for the chosen machine.
#[derive(Clone)]
enum Preview {
    Loading,
    Ready(Box<ProjectMovePreview>),
    Failed(String),
}

pub(crate) struct MoveProjectDialog {
    store: Entity<WorkspaceStore>,
    project_id: String,
    selected: Option<String>,
    preview: Option<Preview>,
    root: Entity<InputState>,
    include_ignored: bool,
    busy: bool,
    error: Option<String>,
    _spaces: Option<gpui::Subscription>,
}

/// Open the move of `project_id`, on the machine `store` shows.
pub(crate) fn open(
    store: Entity<WorkspaceStore>,
    project_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    if store.read(cx).project_move(&project_id).is_some() {
        open_progress(store, project_id, window, cx);
        return;
    }
    let Some((project_name, _)) = store.read(cx).project_summary(&project_id) else {
        return;
    };
    let title = crate::tr!("machines.move.title", project = project_name.clone()).into_owned();
    let spaces = crate::remote::spaces::for_store(&store, cx);
    let dialog = cx.new(|cx| MoveProjectDialog {
        // The group's machines may arrive after the dialog opens.
        _spaces: spaces.map(|spaces| cx.observe(&spaces, |_, _, cx| cx.notify())),
        store,
        project_id,
        selected: None,
        preview: None,
        root: cx.new(|cx| InputState::new(window, cx)),
        include_ignored: true,
        busy: false,
        error: None,
    });
    window.open_dialog(cx, move |builder, _, cx| {
        let content = dialog.clone();
        builder
            .sheet()
            .title(title.clone())
            .content(move |body, _, _| body.child(content.clone()))
            .footer(render_footer(&dialog, cx))
    });
}

impl MoveProjectDialog {
    fn choose(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(machine.as_str()) {
            return;
        }
        self.selected = Some(machine.clone());
        self.preview = Some(Preview::Loading);
        self.error = None;
        cx.notify();
        window.refresh();
        self.ask_preview(machine, window, cx);
    }

    fn ask_preview(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        let task = self.store.update(cx, |store, cx| {
            store.project_move_preview(self.project_id.clone(), machine.clone(), cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let answer = task.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                // An answer for a machine no longer chosen is not this one's.
                if dialog.selected.as_deref() != Some(machine.as_str()) {
                    return;
                }
                dialog.preview = Some(match answer {
                    Ok(preview) => {
                        dialog.root.update(cx, |input, cx| {
                            input.set_value(preview.default_root.clone(), window, cx)
                        });
                        Preview::Ready(Box::new(preview))
                    }
                    Err(error) => Preview::Failed(error.message),
                });
                cx.notify();
                window.refresh();
            });
        })
        .detach();
    }

    fn ready(&self) -> Option<&ProjectMovePreview> {
        match &self.preview {
            Some(Preview::Ready(preview)) => Some(preview),
            _ => None,
        }
    }

    /// Whether Move may be pressed: a machine is chosen, its preview names
    /// nothing in the way, and nothing is in flight.
    fn can_move(&self) -> bool {
        !self.busy
            && self.selected.is_some()
            && self
                .ready()
                .is_some_and(|preview| preview.blockers.is_empty())
    }

    /// Move, after asking first when the move goes through a relay.
    fn request_move(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.ready().filter(|_| self.can_move()) else {
            return;
        };
        if !preview.relayed {
            self.submit(window, cx);
            return;
        }
        let size = format_size(preview.bytes);
        let dialog = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let dialog = dialog.clone();
            alert
                .title(crate::tr!("machines.move.relay_title"))
                .description(crate::tr!(
                    "machines.move.relay_description",
                    size = size.clone()
                ))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("machines.move.action"))
                        .cancel_text(crate::tr!("sidebar.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let dialog = dialog.clone();
                    window.defer(cx, move |window, cx| {
                        dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                    });
                    true
                })
        });
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(machine) = self.selected.clone().filter(|_| self.can_move()) else {
            return;
        };
        let root = self.root.read(cx).value().trim().to_owned();
        if root.is_empty() {
            self.error = Some(crate::tr!("machines.move.root_required").into_owned());
            cx.notify();
            return;
        }
        let include_ignored =
            self.include_ignored && self.ready().is_some_and(|p| !p.ignored_files.is_empty());
        self.busy = true;
        self.error = None;
        cx.notify();
        window.refresh();
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::MoveProject {
                    project_id: self.project_id.clone(),
                    machine: machine.clone(),
                    root,
                    include_ignored,
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.busy = false;
                match result {
                    Ok(_) => {
                        window.close_dialog(cx);
                        open_progress(dialog.store.clone(), dialog.project_id.clone(), window, cx);
                    }
                    Err(error) => {
                        let machine_title = machine_name(&dialog.store, &machine, cx);
                        if error.code == "move_blocked" {
                            dialog.preview = Some(Preview::Loading);
                            dialog.ask_preview(machine.clone(), window, cx);
                        }
                        dialog.error = Some(refusal_sentence(&error, &machine_title));
                        cx.notify();
                        window.refresh();
                    }
                }
            });
        })
        .detach();
    }

    fn render_machines(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let machines: Vec<_> = group_machines(&self.store, cx)
            .into_iter()
            .filter(|machine| !machine.this_machine)
            .collect();
        let rows = machines.into_iter().map(|machine| {
            let selectable = machine.accepts_projects == Some(true);
            let selected = self.selected.as_deref() == Some(machine.id.as_str());
            let title = machine_title(&machine);
            let line = match machine.accepts_projects {
                Some(true) => crate::tr!("machines.move.machine_accepts"),
                Some(false) => crate::tr!("machines.move.machine_refuses"),
                None => crate::tr!("machines.move.machine_unsaid"),
            };
            let fill = gpui_base::motion::transition(
                gpui::ElementId::from(SharedString::from(format!(
                    "move-project-machine-fill-{}",
                    machine.id
                ))),
                if selected {
                    cx.theme().list_active
                } else {
                    cx.theme().list_active.opacity(0.)
                },
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            let id = machine.id.clone();
            div()
                .w_full()
                .pt_0p5()
                .child(
                    crate::material::menu_item_frame(
                        false,
                        if selectable {
                            crate::material::ItemFill::Hover
                        } else {
                            crate::material::ItemFill::Inert
                        },
                        cx,
                    )
                    .id(SharedString::from(format!(
                        "move-project-machine-{}",
                        machine.id
                    )))
                    .debug_selector({
                        let id = machine.id.clone();
                        move || format!("move-project-machine-{id}")
                    })
                    .role(gpui::Role::ListBoxOption)
                    .aria_label(format!("{title}: {line}"))
                    .aria_selected(selected)
                    .w_full()
                    .bg(fill)
                    .when(selectable, |row| {
                        row.cursor_pointer()
                            .on_click(cx.listener(move |dialog, _, window, cx| {
                                dialog.choose(id.clone(), window, cx);
                            }))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_body(false, cx)
                                    .when(!selectable, |title| {
                                        title.text_color(cx.theme().muted_foreground)
                                    })
                                    .child(crate::widgets::fade_text(title.clone())),
                            )
                            .child(
                                div()
                                    .text_secondary(false, cx)
                                    .child(crate::widgets::fade_text(line)),
                            ),
                    ),
                )
                .into_any_element()
        });
        v_flex()
            .id("move-project-machines")
            .role(gpui::Role::ListBox)
            .aria_label(crate::tr!("machines.move.machines"))
            .w_full()
            .max_h(gpui::px(240.))
            .overflow_y_scroll()
            .child(v_flex().w_full().mt(gpui::rems(-0.125)).children(rows))
            .autoscroll_target()
            .into_any_element()
    }

    fn render_preview(
        &self,
        preview: &ProjectMovePreview,
        machine: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = |text: String| div().text_secondary(false, cx).child(text);
        let section = |caption: SharedString, body: AnyElement| {
            v_flex()
                .gap_1()
                .child(crate::material::group_caption(caption, cx))
                .child(body)
        };
        let placeholder = if preview.default_root.is_empty() {
            crate::tr!("machines.move.root_needed", machine = machine)
        } else {
            crate::tr!("machines.move.root_placeholder")
        };
        let location = v_flex()
            .gap_1()
            .child(
                div().debug_selector(|| "move-project-root".into()).child(
                    Input::new(&self.root)
                        .placeholder(placeholder)
                        .aria_label(crate::tr!("machines.move.location"))
                        .disabled(self.busy),
                ),
            )
            .child(muted(
                crate::tr!("machines.move.root_hint", machine = machine).into_owned(),
            ));
        let size = format_size(preview.bytes);
        let code = if preview.git {
            let mut line = crate::tr!(
                "machines.move.code_git",
                branches = preview.branches,
                size = size
            )
            .into_owned();
            if preview.dirty_checkouts > 0 {
                line.push_str(&crate::tr!(
                    "machines.move.code_dirty",
                    count = preview.dirty_checkouts
                ));
            }
            line
        } else {
            crate::tr!("machines.move.code_folder", size = size).into_owned()
        };
        let mut threads =
            crate::tr!("machines.move.threads_count", count = preview.threads).into_owned();
        if preview.archived_threads > 0 {
            threads.push_str(&crate::tr!(
                "machines.move.threads_archived",
                count = preview.archived_threads
            ));
        }
        let ignored = (!preview.ignored_files.is_empty()).then(|| {
            let count = preview.ignored_files.len();
            let more = count.saturating_sub(IGNORED_SHOWN);
            v_flex()
                .gap_1()
                .child(
                    div()
                        .debug_selector(|| "move-project-ignored".into())
                        .child(
                            Checkbox::new("move-project-ignored")
                                .checked(self.include_ignored)
                                .disabled(self.busy)
                                .label(
                                    crate::tr!("machines.move.ignored_include", count = count)
                                        .into_owned(),
                                )
                                .on_click(cx.listener(|dialog, checked: &bool, _, cx| {
                                    dialog.include_ignored = *checked;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    v_flex()
                        .pl_6()
                        .text_code(cx)
                        .text_color(cx.theme().muted_foreground)
                        .children(
                            preview
                                .ignored_files
                                .iter()
                                .take(IGNORED_SHOWN)
                                .map(|file| div().child(crate::widgets::fade_text(file.clone()))),
                        )
                        .when(more > 0, |list| {
                            list.child(div().text_secondary(false, cx).child(
                                crate::tr!("machines.move.ignored_more", count = more).into_owned(),
                            ))
                        }),
                )
                .child(muted(crate::tr!("machines.move.ignored_note").into_owned()))
        });
        let blockers = (!preview.blockers.is_empty()).then(|| {
            section(
                crate::tr!("machines.move.blocked").into(),
                v_flex()
                    .gap_0p5()
                    .debug_selector(|| "move-project-blockers".into())
                    .children(preview.blockers.iter().map(|blocker| {
                        div()
                            .text_secondary(false, cx)
                            .text_color(cx.theme().danger)
                            .child(blocker_sentence(blocker, machine))
                    }))
                    .into_any_element(),
            )
        });
        v_flex()
            .gap_3()
            .child(section(
                crate::tr!("machines.move.location").into(),
                location.into_any_element(),
            ))
            .child(section(
                crate::tr!("machines.move.code").into(),
                muted(code).into_any_element(),
            ))
            .child(section(
                crate::tr!("machines.move.threads").into(),
                muted(threads).into_any_element(),
            ))
            .children(ignored.map(|ignored| {
                section(
                    crate::tr!("machines.move.ignored").into(),
                    ignored.into_any_element(),
                )
            }))
            .when(preview.relayed, |column| {
                column.child(section(
                    crate::tr!("machines.move.path").into(),
                    muted(crate::tr!("machines.move.relayed").into_owned()).into_any_element(),
                ))
            })
            .children(blockers)
            .into_any_element()
    }
}

impl Render for MoveProjectDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let machines = self.render_machines(window, cx);
        let machine = self
            .selected
            .clone()
            .map(|id| machine_name(&self.store, &id, cx));
        let lower = match (&self.preview, &machine) {
            (Some(Preview::Ready(preview)), Some(machine)) => {
                let preview = preview.clone();
                Some(self.render_preview(&preview, machine, cx))
            }
            (Some(Preview::Loading), _) => Some(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("machines.move.asking"))
                    .into_any_element(),
            ),
            (Some(Preview::Failed(reason)), _) => Some(
                div()
                    .text_secondary(false, cx)
                    .text_color(cx.theme().danger)
                    .child(crate::tr!(
                        "machines.move.preview_failed",
                        reason = reason.clone()
                    ))
                    .into_any_element(),
            ),
            _ => None,
        };
        v_flex()
            .gap_3()
            .child(crate::material::group_caption(
                crate::tr!("machines.move.machines"),
                cx,
            ))
            .child(machines)
            .children(lower)
            .children(machine.map(|machine| {
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("machines.move.note", machine = machine))
            }))
            .children(
                crate::motion::shown(
                    "move-project-working",
                    self.busy.then_some(()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|working| {
                    working.fold(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("machines.move.working")),
                    )
                }),
            )
            .children(
                crate::motion::shown("move-project-error", self.error.clone(), SHOW, window, cx)
                    .map(|error| {
                        error.fold(
                            div()
                                .text_secondary(false, cx)
                                .text_color(cx.theme().danger)
                                .child(error.value.clone()),
                        )
                    }),
            )
    }
}

fn render_footer(dialog: &Entity<MoveProjectDialog>, cx: &App) -> AnyElement {
    let ready = dialog.read(cx).can_move();
    let submit = dialog.clone();
    DialogActions::new()
        .child(
            Button::new("move-project-cancel")
                .label(crate::tr!("sidebar.cancel"))
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .child(
            Button::new("move-project-apply")
                .debug_selector(|| "move-project-apply".into())
                .primary()
                .disabled(!ready)
                .label(crate::tr!("machines.move.action"))
                .on_click(move |_, window, cx| {
                    submit.update(cx, |dialog, cx| dialog.request_move(window, cx));
                }),
        )
        .into_any_element()
}

/// What stands in a move's way, in a sentence.
fn blocker_sentence(blocker: &MoveBlocker, machine: &str) -> String {
    match blocker {
        MoveBlocker::QuickChat => crate::tr!("machines.move.blocker.quick_chat"),
        MoveBlocker::InsideRepository => crate::tr!("machines.move.blocker.inside_repository"),
        MoveBlocker::ThreadsRunning(count) => {
            crate::tr!("machines.move.blocker.running", count = count)
        }
        MoveBlocker::PendingApprovals(count) => {
            crate::tr!("machines.move.blocker.approvals", count = count)
        }
        MoveBlocker::QueuedMessages(count) => {
            crate::tr!("machines.move.blocker.queued", count = count)
        }
        MoveBlocker::TerminalsOpen(count) => {
            crate::tr!("machines.move.blocker.terminals", count = count)
        }
        MoveBlocker::MergeInProgress { path } => {
            crate::tr!("machines.move.blocker.merge", path = path)
        }
        MoveBlocker::TargetRefuses => {
            crate::tr!("machines.move.blocker.refuses", machine = machine)
        }
        MoveBlocker::TargetUnreachable => {
            crate::tr!("machines.move.blocker.unreachable", machine = machine)
        }
        MoveBlocker::TargetVersion => {
            crate::tr!("machines.move.blocker.version", machine = machine)
        }
    }
    .into_owned()
}

/// A refused Move, in a sentence for its code; the host's own words for a
/// code this client has none for.
fn refusal_sentence(error: &ProtocolError, machine: &str) -> String {
    let key = match error.code.as_str() {
        "invalid_move_root" => {
            return crate::tr!("machines.move.refused.invalid_root", reason = error.message)
                .into_owned();
        }
        "thread_exists" => "machines.move.refused.thread_exists",
        "move_blocked" => "machines.move.refused.blocked",
        "move_in_progress" => "machines.move.refused.in_progress",
        tcode_client::machines::MACHINE_REFUSES => "machines.move.refused.refuses",
        tcode_client::machines::MACHINE_UNREACHABLE => "machines.move.refused.unreachable",
        tcode_client::machines::PROTOCOL_MISMATCH => "machines.move.refused.version",
        tcode_client::machines::NOT_MEMBER => "machines.move.refused.not_member",
        tcode_client::machines::NO_GROUP => "machines.move.refused.no_group",
        _ => {
            return crate::tr!("machines.move.refused.other", reason = error.message).into_owned();
        }
    };
    crate::tr!(key, machine = machine).into_owned()
}

/// The move of `project_id` as it goes on, until it ends: closable at any
/// time, the move going on without it.
struct MoveProgress {
    store: Entity<WorkspaceStore>,
    project_id: String,
    project_name: String,
    /// The last of the move this view saw, so it can close as the move ends.
    last: Option<ProjectMoveStatus>,
    error: Option<String>,
    _subscription: gpui::Subscription,
}

/// Show the move of `project_id` on `store`'s machine.
pub(crate) fn open_progress(
    store: Entity<WorkspaceStore>,
    project_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some((project_name, _)) = store.read(cx).project_summary(&project_id) else {
        return;
    };
    let title = crate::tr!(
        "machines.move.progress_title",
        project = project_name.clone()
    )
    .into_owned();
    let progress = cx.new(|cx| MoveProgress {
        _subscription: observe_store_topics(&store, &[TopicKind::ProjectMoves], cx),
        last: store.read(cx).project_move(&project_id).cloned(),
        store,
        project_id,
        project_name,
        error: None,
    });
    window.open_dialog(cx, move |builder, _, cx| {
        let content = progress.clone();
        builder
            .title(title.clone())
            .content(move |body, _, _| body.child(content.clone()))
            .footer(render_progress_footer(&progress, cx))
    });
}

impl MoveProgress {
    fn status(&self, cx: &App) -> Option<ProjectMoveStatus> {
        self.store.read(cx).project_move(&self.project_id).cloned()
    }

    fn report(
        &mut self,
        task: gpui::Task<Result<tcode_protocol::CommandResponse, ProtocolError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = task.await {
                let _ = this.update_in(cx, |progress, window, cx| {
                    progress.error = Some(
                        crate::tr!("machines.move.refused.other", reason = error.message)
                            .into_owned(),
                    );
                    cx.notify();
                    window.refresh();
                });
            }
        })
        .detach();
    }

    fn confirm_cancel(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = self.status(cx) else {
            return;
        };
        let machine = machine_name(&self.store, &status.machine, cx);
        let (title, description, ok, variant) = if force {
            (
                crate::tr!(
                    "machines.move.force_title",
                    project = self.project_name.clone()
                ),
                crate::tr!("machines.move.force_description", machine = machine),
                crate::tr!("machines.move.force"),
                ButtonVariant::Danger,
            )
        } else {
            (
                crate::tr!(
                    "machines.move.cancel_title",
                    project = self.project_name.clone()
                ),
                crate::tr!("machines.move.cancel_description", machine = machine),
                crate::tr!("machines.move.cancel"),
                ButtonVariant::Danger,
            )
        };
        let (title, description, ok) = (
            title.into_owned(),
            description.into_owned(),
            ok.into_owned(),
        );
        let progress = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let progress = progress.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .button_props(
                    DialogButtons::default()
                        .ok_variant(variant)
                        .ok_text(ok.clone())
                        .cancel_text(crate::tr!("machines.move.keep_moving"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    progress.update(cx, |progress, cx| {
                        let task = progress.store.update(cx, |store, cx| {
                            store.cancel_project_move(progress.project_id.clone(), force, cx)
                        });
                        progress.report(task, window, cx);
                    });
                    true
                })
        });
    }

    /// Try a paused move again. The host resumes a move it holds for the
    /// same machine whatever root and option are sent; this client may not
    /// be the one that started it, so it sends what the host listed.
    fn try_again(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = self.status(cx) else {
            return;
        };
        self.error = None;
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::MoveProject {
                    project_id: status.project_id.clone(),
                    machine: status.machine.clone(),
                    root: String::new(),
                    include_ignored: false,
                },
                cx,
            )
        });
        self.report(task, window, cx);
    }
}

impl Render for MoveProgress {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status(cx);
        if status != self.last {
            // The footer's actions are drawn by the dialog frame.
            window.refresh();
        }
        self.last.clone_from(&status);
        let Some(status) = status else {
            // The window's notice says how it ended.
            return div()
                .py_2()
                .text_secondary(false, cx)
                .child(crate::tr!("machines.move.ended"))
                .into_any_element();
        };
        let machine = machine_name(&self.store, &status.machine, cx);
        let bytes = (status.total > 0).then(|| {
            crate::tr!(
                "machines.move.bytes",
                done = format_size(status.done),
                total = format_size(status.total)
            )
            .into_owned()
        });
        v_flex()
            .gap_3()
            .py_2()
            .child(
                div()
                    .text_body(false, cx)
                    .debug_selector(|| "move-project-status".into())
                    .child(status_line(&status, &machine)),
            )
            .child(status_bar("move-project-progress", &status, cx))
            .children(bytes.map(|bytes| div().text_secondary(false, cx).child(bytes)))
            .when(status.commit_unknown, |column| {
                column.child(div().text_secondary(false, cx).child(crate::tr!(
                    "machines.move.commit_unknown",
                    machine = machine
                )))
            })
            .children(
                crate::motion::shown("move-progress-error", self.error.clone(), SHOW, window, cx)
                    .map(|error| {
                        error.fold(
                            div()
                                .text_secondary(false, cx)
                                .text_color(cx.theme().danger)
                                .child(error.value.clone()),
                        )
                    }),
            )
            .into_any_element()
    }
}

fn render_progress_footer(progress: &Entity<MoveProgress>, cx: &App) -> AnyElement {
    let status = progress.read(cx).status(cx);
    let (paused, commit_unknown, cancelling) = status.as_ref().map_or((false, false, true), |s| {
        (
            s.phase == MovePhase::Paused,
            s.commit_unknown,
            s.phase == MovePhase::Cancelling,
        )
    });
    let ended = status.is_none();
    let cancel = progress.clone();
    let retry = progress.clone();
    let actions = DialogActions::new().child(
        Button::new("move-progress-close")
            .label(crate::tr!("machines.move.close"))
            .on_click(|_, window, cx| window.close_dialog(cx)),
    );
    if ended {
        return actions.into_any_element();
    }
    actions
        .when(paused && !commit_unknown, |actions| {
            actions.child(
                Button::new("move-progress-retry")
                    .label(crate::tr!("machines.move.try_again"))
                    .on_click(move |_, window, cx| {
                        retry.update(cx, |progress, cx| progress.try_again(window, cx));
                    }),
            )
        })
        .child(if commit_unknown {
            Button::new("move-progress-force")
                .danger()
                .disabled(cancelling)
                .label(crate::tr!("machines.move.force"))
                .on_click(move |_, window, cx| {
                    cancel.update(cx, |progress, cx| progress.confirm_cancel(true, window, cx));
                })
        } else {
            Button::new("move-progress-cancel")
                .disabled(cancelling)
                .label(crate::tr!("machines.move.cancel"))
                .on_click(move |_, window, cx| {
                    cancel.update(cx, |progress, cx| {
                        progress.confirm_cancel(false, window, cx)
                    });
                })
        })
        .into_any_element()
}

#[cfg(test)]
mod tests;
