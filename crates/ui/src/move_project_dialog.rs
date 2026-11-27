//! Move to machine: hand a project and all its threads to another machine of
//! the group. The machine that holds the project asks the other one what the
//! move would take, sends everything there and lets the project go only once
//! the other machine has it; this client chooses, shows and can cancel.

use crate::overlay::{DialogActions, DialogButtons, OverlayExt as _};
use crate::scroll::AutoscrollTarget as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::checkbox::Checkbox;
use crate::widgets::input::{Input, InputState};
use crate::widgets::progress::Progress;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _,
};
use gpui_base::{StyledExt as _, v_flex};
use tcode_protocol::{
    Command, EndedProjectMove, MoveBlocker, MoveOutcome, MovePause, MovePhase, ProjectMovePreview,
    ProjectMoveStatus, ProtocolError,
};

use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::thread_export::format_size;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How many of the ignored files the dialog names before it counts the rest.
const IGNORED_SHOWN: usize = 6;

/// Whether a project of `store`'s machine could be offered a move: a full
/// device of a machine whose group has another member.
pub(crate) fn offers_move(store: &Entity<WorkspaceStore>, cx: &mut App) -> bool {
    store.read(cx).scope().is_full()
        && crate::machines::group_machines(store, cx)
            .iter()
            .any(|machine| !machine.this_machine)
}

/// Where a move's line is drawn: the project's row in the sidebar has room
/// for a few words, the progress dialog for the whole sentence.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineForm {
    Row,
    Dialog,
}

/// How far a move is, in a line: the row under the project and the progress
/// dialog say the same, the row why a move waits in short.
pub(crate) fn status_line(status: &ProjectMoveStatus, machine: &str, form: LineForm) -> String {
    if form == LineForm::Row && status.phase == MovePhase::Paused {
        return match &status.paused {
            Some(MovePause::Unreachable) => {
                crate::tr!("machines.move.paused_short.unreachable", machine = machine)
            }
            Some(MovePause::Refused) => {
                crate::tr!("machines.move.paused_short.refused", machine = machine)
            }
            Some(MovePause::NoGroup) => crate::tr!("machines.move.paused_short.no_group"),
            Some(MovePause::Version) => crate::tr!("machines.move.paused_short.version"),
            Some(MovePause::NotMember) => crate::tr!("machines.move.paused_short.not_member"),
            Some(MovePause::Failed { .. }) | None => crate::tr!("machines.move.status_paused_bare"),
        }
        .into_owned();
    }
    match status.phase {
        MovePhase::Preparing => crate::tr!("machines.move.status_preparing", machine = machine),
        MovePhase::SendingCode | MovePhase::SendingThreads => crate::tr!(
            "machines.move.status_sending",
            machine = machine,
            percent = percent(status).round() as u32
        ),
        MovePhase::Committing => crate::tr!("machines.move.status_finishing", machine = machine),
        MovePhase::Paused => match &status.paused {
            Some(MovePause::Unreachable) => {
                crate::tr!("machines.move.paused.unreachable", machine = machine)
            }
            Some(MovePause::Refused) => {
                crate::tr!("machines.move.paused.refused", machine = machine)
            }
            Some(MovePause::NoGroup) => crate::tr!("machines.move.paused.no_group"),
            Some(MovePause::Version) => {
                crate::tr!("machines.move.paused.version", machine = machine)
            }
            Some(MovePause::NotMember) => {
                crate::tr!("machines.move.paused.not_member", machine = machine)
            }
            Some(MovePause::Failed { message }) => {
                crate::tr!("machines.move.paused.failed", reason = message.as_str())
            }
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

/// A section's heading, flush with what it heads.
fn heading(label: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .text_caption(cx)
        .child(crate::widgets::fade_text(label.into()))
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
        // While the request is out the sheet cannot be closed: its answer
        // closes the topmost dialog, which must still be this one.
        let busy = dialog.read(cx).busy;
        builder
            .sheet()
            .keyboard(!busy)
            .close_button(!busy)
            .overlay_closable(!busy)
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

    /// Whether Move may be pressed: a machine is chosen, its preview has
    /// come, and nothing is in flight. What stands in the way is the host's
    /// to refuse, and the dialog shows what it says.
    fn can_move(&self) -> bool {
        !self.busy && self.selected.is_some() && self.ready().is_some()
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
                        let machine_title = crate::machines::machine_in_groups(
                            std::slice::from_ref(&dialog.store),
                            &machine,
                            cx,
                        )
                        .name;
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
        let machines: Vec<_> = crate::machines::group_machines(&self.store, cx)
            .into_iter()
            .filter(|machine| !machine.this_machine)
            .collect();
        let rows = machines.into_iter().map(|machine| {
            let selected = self.selected.as_deref() == Some(machine.id.as_str());
            let title = machine.title().to_owned();
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
                    crate::material::menu_item_frame(false, crate::material::ItemFill::Hover, cx)
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
                        .cursor_pointer()
                        .on_click(cx.listener(move |dialog, _, window, cx| {
                            dialog.choose(id.clone(), window, cx);
                        }))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_body(false, cx)
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
            v_flex().gap_1().child(heading(caption, cx)).child(body)
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
        let code = if preview.blockers.contains(&MoveBlocker::InsideRepository) {
            crate::tr!("machines.move.code_inside_repository").into_owned()
        } else if preview.git {
            let mut line = if preview.branches == 1 {
                crate::tr!("machines.move.code_git_one", size = size)
            } else {
                crate::tr!(
                    "machines.move.code_git",
                    branches = preview.branches,
                    size = size
                )
            }
            .into_owned();
            match preview.dirty_checkouts {
                0 => {}
                1 => line.push_str(&crate::tr!("machines.move.code_dirty_one")),
                count => line.push_str(&crate::tr!("machines.move.code_dirty", count = count)),
            }
            line
        } else {
            crate::tr!("machines.move.code_folder", size = size).into_owned()
        };
        let mut threads = if preview.threads == 1 {
            crate::tr!("machines.move.threads_count_one")
        } else {
            crate::tr!("machines.move.threads_count", count = preview.threads)
        }
        .into_owned();
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
                                    if count == 1 {
                                        crate::tr!("machines.move.ignored_include_one")
                                    } else {
                                        crate::tr!("machines.move.ignored_include", count = count)
                                    }
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
        let machine = self.selected.clone().map(|id| {
            crate::machines::machine_in_groups(std::slice::from_ref(&self.store), &id, cx).name
        });
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
            .child(heading(crate::tr!("machines.move.machines"), cx))
            .child(machines)
            .children(lower)
            .children(machine.map(|machine| {
                let folder = self.ready().is_some_and(|preview| !preview.git);
                div().text_secondary(false, cx).child(if folder {
                    crate::tr!("machines.move.note_folder", machine = machine)
                } else {
                    crate::tr!("machines.move.note", machine = machine)
                })
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
                .disabled(dialog.read(cx).busy)
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
        MoveBlocker::ThreadsRunning(count) => counted("machines.move.blocker.running", *count),
        MoveBlocker::PendingApprovals(count) => counted("machines.move.blocker.approvals", *count),
        MoveBlocker::QueuedMessages(count) => counted("machines.move.blocker.queued", *count),
        MoveBlocker::TerminalsOpen(count) => counted("machines.move.blocker.terminals", *count),
        MoveBlocker::MergeInProgress { path } => {
            crate::tr!("machines.move.blocker.merge", path = path)
        }
        MoveBlocker::MissingCheckout { thread, path } => {
            crate::tr!(
                "machines.move.blocker.checkout_missing",
                thread = thread,
                path = path
            )
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

/// `key`'s sentence for `count`, its `_one` form for one.
fn counted(key: &str, count: u32) -> std::borrow::Cow<'static, str> {
    if count == 1 {
        crate::tr!(&format!("{key}_one"))
    } else {
        crate::tr!(key, count = count)
    }
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
    /// The last of the move this view saw, which names the move whose end it
    /// waits for and is shown while the store has no status for it.
    last: Option<ProjectMoveStatus>,
    /// Whether the host had said the move ended when this view last drew.
    ended: bool,
    /// The question this view is asking in place of the progress, if any:
    /// whether to cancel the move, `force` when its outcome is unknown.
    asking: Option<bool>,
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
    let progress = cx.new(|cx| {
        store.update(cx, |store, _| store.watch_project_move(&project_id));
        cx.on_release(|progress: &mut MoveProgress, cx| {
            progress.store.update(cx, |store, _| {
                store.unwatch_project_move(&progress.project_id)
            });
        })
        .detach();
        MoveProgress {
            _subscription: observe_store_topics(&store, &[TopicKind::ProjectMoves], cx),
            last: store.read(cx).project_move(&project_id).cloned(),
            ended: false,
            asking: None,
            store,
            project_id,
            project_name,
            error: None,
        }
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

    /// How the host says this view's move ended, once it has. A view that has
    /// not seen its move listed yet waits for it: an earlier move of the
    /// project may have ended.
    fn outcome(&self, cx: &App) -> Option<EndedProjectMove> {
        let move_id = self
            .status(cx)
            .or_else(|| self.last.clone())
            .map(|status| status.move_id)?;
        self.store.read(cx).ended_project_move(&move_id).cloned()
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
                        match error.code.as_str() {
                            // The move ended before the cancel reached the host.
                            "move_committed" | "no_project_move" => {
                                crate::tr!("machines.move.already_ended")
                            }
                            _ => crate::tr!("machines.move.refused.other", reason = error.message),
                        }
                        .into_owned(),
                    );
                    cx.notify();
                    window.refresh();
                });
            }
        })
        .detach();
    }

    /// Turn this view into the question whether to cancel the move.
    fn ask(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.asking = Some(force);
        cx.notify();
        window.refresh();
    }

    /// Answer the question: send the cancel, or go on with the move.
    fn answer(&mut self, cancel: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(force) = self.asking.take() else {
            return;
        };
        cx.notify();
        window.refresh();
        if cancel {
            let task = self.store.update(cx, |store, cx| {
                store.cancel_project_move(self.project_id.clone(), force, cx)
            });
            self.report(task, window, cx);
        }
    }

    /// Try a paused move again.
    fn try_again(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::ResumeProjectMove {
                    project_id: self.project_id.clone(),
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
        let outcome = self.outcome(cx);
        let ended = outcome.is_some();
        if (status.is_some() && status != self.last) || ended != self.ended {
            // The footer's actions are drawn by the dialog frame.
            window.refresh();
        }
        self.ended = ended;
        if ended {
            // A move that has ended has nothing left to cancel.
            self.asking = None;
        }
        if let Some(outcome) = outcome {
            let machine = crate::machines::machine_in_groups(
                std::slice::from_ref(&self.store),
                &outcome.machine,
                cx,
            )
            .name;
            let line = match outcome.outcome {
                MoveOutcome::Moved => {
                    crate::tr!("machines.move.ended_moved", machine = machine)
                }
                MoveOutcome::Cancelled { forced: true } => {
                    crate::tr!("machines.move.ended_forced", machine = machine)
                }
                MoveOutcome::Cancelled { forced: false } => {
                    crate::tr!("machines.move.ended_cancelled")
                }
            };
            return v_flex()
                .gap_3()
                .py_2()
                .child(
                    div()
                        .text_body(false, cx)
                        .debug_selector(|| "move-project-status".into())
                        .child(line),
                )
                .children(self.error.clone().map(|error| {
                    div()
                        .text_secondary(false, cx)
                        .text_color(cx.theme().danger)
                        .child(error)
                }))
                .into_any_element();
        }
        if status.is_some() {
            self.last.clone_from(&status);
        }
        // Until the host says how it ended, the move reads as it last did.
        let Some(status) = status.or_else(|| self.last.clone()) else {
            return div().into_any_element();
        };
        let machine = crate::machines::machine_in_groups(
            std::slice::from_ref(&self.store),
            &status.machine,
            cx,
        )
        .name;
        if let Some(force) = self.asking {
            let (title, description) = if force {
                (
                    crate::tr!(
                        "machines.move.force_title",
                        project = self.project_name.clone()
                    ),
                    crate::tr!("machines.move.force_description", machine = machine),
                )
            } else {
                (
                    crate::tr!(
                        "machines.move.cancel_title",
                        project = self.project_name.clone()
                    ),
                    crate::tr!("machines.move.cancel_description", machine = machine),
                )
            };
            return v_flex()
                .gap_3()
                .py_2()
                .child(
                    div()
                        .text_body(false, cx)
                        .font_semibold()
                        .child(title.into_owned()),
                )
                .child(
                    div()
                        .text_secondary(false, cx)
                        .child(description.into_owned()),
                )
                .into_any_element();
        }
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
                    .child(status_line(&status, &machine, LineForm::Dialog)),
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
    let ended = progress.read(cx).outcome(cx).is_some();
    if let Some(force) = progress.read(cx).asking.filter(|_| !ended) {
        let (keep, confirm) = (progress.clone(), progress.clone());
        return DialogActions::new()
            .child(
                Button::new("move-progress-keep")
                    .debug_selector(|| "move-progress-keep".into())
                    .label(crate::tr!("machines.move.keep_moving"))
                    .on_click(move |_, window, cx| {
                        keep.update(cx, |progress, cx| progress.answer(false, window, cx));
                    }),
            )
            .child(
                Button::new("move-progress-confirm")
                    .debug_selector(|| "move-progress-confirm".into())
                    .danger()
                    .label(if force {
                        crate::tr!("machines.move.force")
                    } else {
                        crate::tr!("machines.move.cancel")
                    })
                    .on_click(move |_, window, cx| {
                        confirm.update(cx, |progress, cx| progress.answer(true, window, cx));
                    }),
            )
            .into_any_element();
    }
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
                    cancel.update(cx, |progress, cx| progress.ask(true, window, cx));
                })
        } else {
            Button::new("move-progress-cancel")
                .debug_selector(|| "move-progress-cancel".into())
                .disabled(cancelling)
                .label(crate::tr!("machines.move.cancel"))
                .on_click(move |_, window, cx| {
                    cancel.update(cx, |progress, cx| progress.ask(false, window, cx));
                })
        })
        .into_any_element()
}

#[cfg(test)]
mod tests;
