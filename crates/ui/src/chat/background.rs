//! The thread's background work: each piece its harness runs in the
//! background with how long it has run, how it stands and a stop where the
//! harness has one. On a wide window each piece is a row of the rail card
//! that lists its kind (a command in Commands, a subagent in Agents); without
//! the rail a pill opens them all as one list over the timeline, or in a
//! sheet on a phone.

use std::rc::Rc;
use std::time::Duration;

use agent::{BackgroundKind, BackgroundStop};
use gpui::{
    AnyElement, App, Entity, InteractiveElement as _, IntoElement as _, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_protocol::BackgroundTaskStatus;

use super::ChatView;
use crate::icon::{Icon, IconName};
use crate::overlay::{DialogButtons, OverlayExt as _};
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::spinner::Spinner;
use crate::widgets::tooltip::{Tooltip, TooltipTrigger as _};

/// How long a row takes to fold in or out.
const ROWS: Duration = Duration::from_millis(200);

/// How long a row's state takes to cross-fade to its next, and its
/// highlight to come or go.
const CHANGE: Duration = Duration::from_millis(200);

/// A thread's background work, as its host reports it.
#[derive(Clone, PartialEq)]
pub(in crate::chat) struct Background {
    pub(in crate::chat) harness: String,
    /// What of it the harness stops.
    stop: BackgroundStop,
    pub(in crate::chat) rows: Vec<BackgroundTaskStatus>,
}

impl Background {
    /// The thread's background work, while its harness runs any.
    pub(in crate::chat) fn of(store: &WorkspaceStore, session_id: &str, cx: &App) -> Option<Self> {
        let status = store.session_status_for(Some(session_id), cx)?;
        if status.background_tasks.is_empty() {
            return None;
        }
        Some(Self {
            harness: store.harness_name_for(Some(session_id), cx),
            stop: status
                .provider
                .caps()
                .background
                .map_or(BackgroundStop::None, |control| control.stop),
            rows: status.background_tasks.clone(),
        })
    }

    /// How many pieces still run: listed, not ended.
    pub(in crate::chat) fn running(&self) -> usize {
        self.rows.iter().filter(|row| !row.ended).count()
    }

    /// What the header says beside the title: whose work it is and how much
    /// of it runs.
    pub(in crate::chat) fn summary(&self) -> SharedString {
        crate::tr!(
            "background.running",
            harness = self.harness.as_str(),
            count = self.running()
        )
        .into_owned()
        .into()
    }

    /// The pieces a stop reaches now: listed, running, with no stop out.
    fn stoppable(&self) -> Vec<&BackgroundTaskStatus> {
        if !self.stop.stops() {
            return Vec::new();
        }
        self.rows
            .iter()
            .filter(|row| row.listed && !row.ended && row.stopping_since.is_none())
            .collect()
    }
}

/// Closes what holds a list.
pub(in crate::chat) type Leave = Rc<dyn Fn(&mut Window, &mut App)>;

/// Where a list is drawn and what it acts through.
#[derive(Clone)]
pub(in crate::chat) struct Place {
    pub(in crate::chat) store: Entity<WorkspaceStore>,
    pub(in crate::chat) session_id: String,
    pub(in crate::chat) chat: WeakEntity<ChatView>,
    /// Keys the list's elements apart from another drawing of it.
    pub(in crate::chat) key: &'static str,
    /// The piece the timeline asked to point out, by its call.
    pub(in crate::chat) highlight: Option<String>,
    /// Sized for touch: taller rows, and stops that say so.
    pub(in crate::chat) touch: bool,
    /// Closes what holds the list once a row has taken the reader elsewhere.
    pub(in crate::chat) leave: Option<Leave>,
    /// Draws the list again, once a stop has gone unanswered long enough.
    pub(in crate::chat) redraw: Rc<dyn Fn(&mut App)>,
}

impl Place {
    fn go(
        &self,
        window: &mut Window,
        cx: &mut App,
        then: impl FnOnce(&mut ChatView, &mut Window, &mut gpui::Context<ChatView>) + 'static,
    ) {
        if let Some(leave) = &self.leave {
            leave(window, cx);
        }
        let chat = self.chat.clone();
        window.defer(cx, move |window, cx| {
            let _ = chat.update(cx, |chat, cx| then(chat, window, cx));
        });
    }
}

/// The stop for every piece at once, beside the title, wherever the harness
/// stops them. More than one piece asks first: a stop takes a dev server
/// down along with what the reader meant to end.
pub(in crate::chat) fn stop_all(
    background: &Background,
    place: &Place,
    cx: &App,
) -> Option<AnyElement> {
    let stoppable = background.stoppable();
    if stoppable.is_empty() {
        return None;
    }
    let names: Vec<String> = stoppable
        .iter()
        .map(|row| crate::chat::model::one_line(&row.task.name))
        .collect();
    let (store, session_id) = (place.store.clone(), place.session_id.clone());
    Some(
        Button::new(SharedString::from(format!(
            "{}-background-stop-all",
            place.key
        )))
        .debug_selector(|| "background-stop-all".into())
        .ghost()
        .xsmall()
        .touch(place.touch)
        .text_secondary(false, cx)
        .label(crate::tr!("background.stop_all").into_owned())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            let (store, session_id, names) = (store.clone(), session_id.clone(), names.clone());
            if names.len() < 2 {
                store.update(cx, |store, _| {
                    store.stop_background_task(session_id.clone(), None)
                });
                return;
            }
            window.open_alert_dialog(cx, move |alert, _, _| {
                let (store, session_id) = (store.clone(), session_id.clone());
                alert
                    .title(crate::tr!(
                        "background.stop_confirm_title",
                        count = names.len()
                    ))
                    .description(crate::tr!(
                        "background.stop_confirm_description",
                        names = names.join("\n")
                    ))
                    .button_props(
                        DialogButtons::default()
                            .ok_variant(ButtonVariant::Danger)
                            .ok_text(crate::tr!("background.stop_confirm"))
                            .cancel_text(crate::tr!("settings.cancel"))
                            .show_cancel(true),
                    )
                    .on_ok(move |_, _, cx| {
                        store.update(cx, |store, _| {
                            store.stop_background_task(session_id.clone(), None)
                        });
                        true
                    })
            });
        })
        .into_any_element(),
    )
}

/// The list: a row for each piece, folding in and out as the harness
/// starts and ends them; then, where the harness stops none of it, that it
/// does not.
pub(in crate::chat) fn body(
    background: &Background,
    place: &Place,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let rows = background.rows.iter().map(|row| {
        (
            SharedString::from(format!("{}-bg-{}", place.key, row.task.id)),
            row.clone(),
        )
    });
    let rows = crate::motion::keyed(
        SharedString::from(format!("{}-background-rows", place.key)),
        rows,
        ROWS,
        window,
        cx,
        |_, row, window, cx| self::row(background, row, place, window, cx),
    )
    .render(v_flex().w_full().min_w_0());
    let no_stop = (!background.stop.stops()).then(|| {
        div()
            .w_full()
            .px_1()
            .pt_1()
            .text_secondary(place.touch, cx)
            .text_color(cx.theme().muted_foreground)
            .child(crate::tr!(
                "background.no_stop",
                harness = background.harness.as_str()
            ))
    });
    v_flex()
        .w_full()
        .min_w_0()
        .child(rows)
        .children(no_stop)
        .into_any_element()
}

fn kind_icon(kind: BackgroundKind) -> IconName {
    match kind {
        BackgroundKind::Command => IconName::SquareTerminal,
        BackgroundKind::Agent => IconName::Bot,
        BackgroundKind::Other => IconName::CircleDot,
    }
}

/// How a piece stands, as its row says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Running,
    Stopping,
    /// Stopping, and the stop has gone unanswered too long.
    Unanswered,
    Ended,
}

impl Standing {
    fn of(row: &BackgroundTaskStatus, now: u64) -> Self {
        let unanswered = crate::composer::UNANSWERED.as_millis() as u64;
        match row.stopping_since {
            _ if row.ended => Self::Ended,
            Some(since) if now.saturating_sub(since) >= unanswered => Self::Unanswered,
            Some(_) => Self::Stopping,
            None => Self::Running,
        }
    }
}

/// Draws the list again once the stop out for `row` has gone unanswered
/// long enough, while it is still drawn.
fn wake_when_unanswered(
    row: &BackgroundTaskStatus,
    place: &Place,
    now: u64,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(since) = row.stopping_since else {
        return;
    };
    let unanswered = crate::composer::UNANSWERED.as_millis() as u64;
    let left = (since + unanswered).saturating_sub(now);
    if left == 0 {
        return;
    }
    let redraw = place.redraw.clone();
    let id = SharedString::from(format!("{}-bg-wake-{}-{since}", place.key, row.task.id));
    window.use_keyed_state(id, cx, move |window, cx| {
        cx.spawn_in(window, async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(left))
                .await;
            let _ = cx.update(|_, cx| redraw(cx));
        })
    });
}

fn row(
    background: &Background,
    row: &BackgroundTaskStatus,
    place: &Place,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let now = crate::time::now_millis();
    let standing = Standing::of(row, now);
    wake_when_unanswered(row, place, now, window, cx);
    let task = &row.task;
    let id = task.id.clone();
    let key = format!("{}-bg-{}", place.key, id);
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let name = crate::chat::model::one_line(&task.name);
    let highlighted = place.highlight.is_some() && place.highlight == task.item_id;
    let accent = theme.accent;
    let wash = crate::motion::shown(
        SharedString::from(format!("{key}-wash")),
        highlighted.then_some(()),
        CHANGE,
        window,
        cx,
    )
    .map(|shown| accent.opacity(shown.progress()));
    let radius = crate::material::radius_card(cx);
    let elapsed = (standing != Standing::Ended).then(|| {
        let first_seen = !task.started_reported;
        let clock = crate::widgets::clock::Clock::since(
            SharedString::from(format!("{key}-elapsed")),
            task.started_at,
            if first_seen {
                |elapsed| {
                    format!("≥ {}", crate::chat::model::format_elapsed_seconds(elapsed)).into()
                }
            } else {
                |elapsed| crate::chat::model::format_elapsed_seconds(elapsed).into()
            },
        );
        let harness = background.harness.clone();
        div()
            .id(SharedString::from(format!("{key}-elapsed-wrap")))
            .flex_none()
            .whitespace_nowrap()
            .child(clock)
            .when(first_seen, |clock| {
                clock.with_tooltip(move |_, _| {
                    Tooltip::new(crate::tr!(
                        "background.first_seen",
                        harness = harness.as_str()
                    ))
                })
            })
    });
    let state = crate::motion::cross_fade_part(
        SharedString::from(format!("{key}-state")),
        standing,
        CHANGE,
        window,
        cx,
        |standing, _, cx| {
            let theme = cx.theme();
            match standing {
                Standing::Running | Standing::Unanswered => div().into_any_element(),
                Standing::Stopping => h_flex()
                    .gap_1()
                    .items_center()
                    .whitespace_nowrap()
                    .child(Spinner::new().xsmall().color(theme.muted_foreground))
                    .child(crate::tr!("background.stopping"))
                    .into_any_element(),
                Standing::Ended => div()
                    .whitespace_nowrap()
                    .child(crate::tr!("background.ended"))
                    .into_any_element(),
            }
        },
    );
    let stop =
        (standing == Standing::Running && row.listed && background.stop.stops()).then(|| {
            let (store, session_id, task_id) =
                (place.store.clone(), place.session_id.clone(), id.clone());
            let selector = format!("background-stop-{id}");
            let button = Button::new(SharedString::from(format!("{key}-stop")))
                .debug_selector(move || selector.clone())
                .ghost()
                .xsmall()
                .touch(place.touch)
                .tooltip(crate::tr!("background.stop_task"));
            let button = if place.touch {
                button.label(crate::tr!("background.stop").into_owned())
            } else {
                button.icon(IconName::Square)
            };
            button.on_click(move |_, _, cx| {
                cx.stop_propagation();
                store.update(cx, |store, _| {
                    store.stop_background_task(session_id.clone(), Some(task_id.clone()))
                });
            })
        });
    // A stop gone unanswered too long takes the stop's place with the one
    // way past it: ending the harness's processes.
    let force = (standing == Standing::Unanswered).then(|| {
        let (store, session_id, harness) = (
            place.store.clone(),
            place.session_id.clone(),
            background.harness.clone(),
        );
        Button::new(SharedString::from(format!("{key}-force")))
            .debug_selector(|| "background-force-terminate".into())
            .danger()
            .xsmall()
            .touch(place.touch)
            .label(crate::tr!("background.force_terminate").into_owned())
            .tooltip(crate::tr!(
                "background.unanswered_detail",
                harness = harness.as_str()
            ))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                crate::composer::confirm_force_terminate(
                    &store,
                    session_id.clone(),
                    harness.clone(),
                    window,
                    cx,
                );
            })
    });
    // A subagent running in the background keeps its own transcript, which
    // its row opens; any other row leads to its call in the timeline.
    let transcript = (standing == Standing::Running && task.kind == BackgroundKind::Agent)
        .then(|| {
            let item = task.item_id.as_deref()?;
            place
                .store
                .read(cx)
                .sidebar_sessions()
                .iter()
                .find(|meta| {
                    meta.parent_session_id.as_deref() == Some(place.session_id.as_str())
                        && meta.native_subagent.as_deref() == Some(item)
                })
                .map(|meta| meta.id.clone())
        })
        .flatten();
    let line = h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .items_center()
        .when(place.touch, |line| line.min_h(px(44.)))
        .child(
            Icon::new(kind_icon(task.kind))
                .small()
                .flex_none()
                .text_color(muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .map(|name| {
                    if task.kind == BackgroundKind::Command {
                        name.text_meta_mono(place.touch, cx)
                    } else {
                        name.text_body(place.touch, cx)
                    }
                })
                .text_color(if standing == Standing::Ended {
                    muted
                } else {
                    cx.theme().foreground
                })
                .child(crate::widgets::fade_text(SharedString::from(name.clone()))),
        )
        .child(
            h_flex()
                .flex_none()
                .gap_1()
                .items_center()
                .text_secondary(place.touch, cx)
                .text_color(muted)
                .children(elapsed)
                .child(state)
                .children(stop)
                .children(force),
        );
    let item = task.item_id.clone();
    let place = place.clone();
    let selector = id.clone();
    crate::material::accessible_clickable(
        v_flex(),
        SharedString::from(format!("{key}-row")),
        Role::Button,
        name,
        cx,
    )
    .debug_selector(move || format!("background-row-{selector}"))
    .relative()
    .w_full()
    .min_w_0()
    .px_1()
    .py_1()
    .rounded(radius)
    .when_some(wash, |row, wash| row.bg(wash))
    .when(standing == Standing::Ended, |row| row.opacity(0.6))
    .child(crate::material::row_hover(radius, cx))
    .child(line)
    .map(|row| match (transcript, item) {
        (Some(mirror), _) => row.cursor_pointer().on_click(move |_, window, cx| {
            let mirror = mirror.clone();
            place.go(window, cx, move |chat, window, cx| {
                chat.open_subagent(mirror, window, cx)
            });
        }),
        (None, Some(item)) => row.cursor_pointer().on_click(move |_, window, cx| {
            let item = item.clone();
            place.go(window, cx, move |chat, window, cx| {
                chat.locate_item(&item, window, cx)
            });
        }),
        (None, None) => row,
    })
    .into_any_element()
}

/// How long a piece pointed out from the timeline stays marked.
const POINTED_OUT: Duration = Duration::from_secs(4);

impl ChatView {
    /// The thread's background work, while its harness runs any.
    pub(in crate::chat) fn background(&self, cx: &App) -> Option<Background> {
        Background::of(
            self.workspace_store.read(cx),
            self.session_id.as_deref()?,
            cx,
        )
    }

    /// Where this view draws the list, under `key`.
    pub(in crate::chat) fn background_place(
        &self,
        key: &'static str,
        cx: &mut gpui::Context<Self>,
    ) -> Option<Place> {
        let view = cx.entity().downgrade();
        Some(Place {
            store: self.workspace_store.clone(),
            session_id: self.session_id.clone()?,
            chat: view.clone(),
            key,
            highlight: self.background_highlight.clone(),
            touch: self.window_state.read(cx).compact,
            leave: Some(Rc::new({
                let view = view.clone();
                move |_, cx| {
                    let _ = view.update(cx, |chat, cx| chat.set_background_open(false, cx));
                }
            })),
            redraw: Rc::new(move |cx| {
                let _ = view.update(cx, |_, cx| cx.notify());
            }),
        })
    }

    pub(in crate::chat) fn set_background_open(
        &mut self,
        open: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.background_open != open {
            self.background_open = open;
            cx.notify();
        }
    }

    /// Point out the piece of background work the call `item_id` started:
    /// in the rail where it shows, else in the list opened over the thread.
    pub(in crate::chat) fn reveal_background(
        &mut self,
        item_id: String,
        cx: &mut gpui::Context<Self>,
    ) {
        self.background_highlight = Some(item_id.clone());
        let highlight = self.background_highlight.clone();
        self.rail
            .update(cx, |rail, cx| rail.highlight_background(highlight, cx));
        if !self.body.rail() {
            self.background_open = true;
        }
        self.background_unmark = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(POINTED_OUT).await;
            let _ = this.update(cx, |chat, cx| {
                if chat.background_highlight.as_deref() == Some(item_id.as_str()) {
                    chat.background_highlight = None;
                    chat.rail
                        .update(cx, |rail, cx| rail.highlight_background(None, cx));
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// Bring the call `item_id` into view in the timeline and mark it.
    pub(in crate::chat) fn locate_item(
        &mut self,
        item_id: &str,
        _: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.timeline
            .update(cx, |timeline, cx| timeline.locate(item_id, cx));
    }

    /// A subagent's own transcript, beside the thread or over it.
    pub(in crate::chat) fn open_subagent(
        &mut self,
        mirror: String,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let host = super::side::agent_host(
            &cx.entity().downgrade(),
            None,
            self.session_id.is_some(),
            &self.window_state,
            cx,
        );
        let store = self.workspace_store.clone();
        window.defer(cx, move |window, cx| {
            crate::agents_panel::open_agent(&store, mirror, host, None, window, cx)
        });
    }
}

#[cfg(test)]
mod tests {
    use agent::{AgentEvent, ItemContent, ItemStatus, ThreadItem};
    use gpui::{AppContext as _, Modifiers, TestAppContext, VisualTestContext, px, size};
    use tcode_protocol::{ClientPayload, Command, ServerEvent, Topic};

    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::draw_settled;

    fn task(id: &str, item_id: Option<&str>, name: &str) -> BackgroundTaskStatus {
        BackgroundTaskStatus {
            task: agent::BackgroundTask {
                id: id.into(),
                name: name.into(),
                kind: BackgroundKind::Command,
                item_id: item_id.map(str::to_owned),
                started_at: 1,
                started_reported: false,
            },
            listed: true,
            stopping_since: None,
            ended: false,
        }
    }

    fn stops(thread: &ScriptedThread) -> Vec<Option<String>> {
        std::iter::from_fn(|| thread.outgoing.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    ClientPayload::Command(Command::StopBackgroundTask { task_id, .. }) => {
                        Some(task_id)
                    }
                    _ => None,
                },
            )
            .collect()
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        draw_settled(cx);
    }

    /// A thread's background work is one list: the rail's card where the
    /// rail fits, else a pill's list over the timeline, which the window of
    /// the call that started a piece opens too. A row's stop reaches the
    /// host at once; the stop for all of it asks first.
    #[gpui::test]
    fn background_work_is_one_list_whichever_way_it_is_reached(cx: &mut TestAppContext) {
        let started = AgentEvent::ItemStarted(ThreadItem {
            id: "call_1".into(),
            parent_item_id: None,
            content: ItemContent::CommandExecution {
                command: "npm run dev".into(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::InProgress,
            },
        });
        let events = vec![
            AgentEvent::TurnStarted {
                turn_id: "t1".into(),
            },
            user_event("u1", "Start the dev server and the tests."),
            started,
            AgentEvent::ItemBackgrounded {
                item_id: "call_1".into(),
                parent_item_id: None,
            },
        ];
        let mut thread = ScriptedThread::new(cx, events, 0, true);
        thread.status.background_tasks = vec![
            task("98412", Some("call_1"), "npm run dev"),
            task("98413", None, "cargo watch -x test"),
        ];
        thread.event(
            Topic::SessionStatus {
                session_id: THREAD.into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(thread.status.clone())),
        );
        // Under the window's root, which hosts the dialogs.
        let store = thread.store.clone();
        let window_state = cx.new(|_| crate::window_state::WindowState::default());
        let mut chat = None;
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| ChatView::new(store, window_state, Some(THREAD.to_owned()), window, cx));
            let frame = view.read(cx).frame();
            chat = Some(view);
            gpui_base::Root::new(frame, window, cx)
        });
        let _chat = chat;
        cx.simulate_resize(size(px(1_400.), px(900.)));
        // The background glyphs breathe for as long as the work runs.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        thread.deliver(cx);
        draw_settled(cx);
        while thread.outgoing.try_recv().is_ok() {}

        assert!(
            cx.debug_bounds("rail-card-background").is_some(),
            "the rail lists the work"
        );
        assert!(
            cx.debug_bounds("background-pill").is_none(),
            "no pill beside the rail"
        );
        click(cx, "background-stop-98412");
        assert_eq!(stops(&thread), [Some("98412".to_owned())]);
        click(cx, "background-stop-all");
        assert!(cx.update(|window, cx| crate::overlay::dialog_open(window, cx)));
        assert!(
            stops(&thread).is_empty(),
            "the stop for all of it asks first"
        );
        cx.update(|window, cx| window.close_all_dialogs(cx));
        draw_settled(cx);

        cx.simulate_resize(size(px(900.), px(900.)));
        draw_settled(cx);
        assert!(
            cx.debug_bounds("rail-card-background").is_none(),
            "no rail at this width"
        );
        assert!(
            cx.debug_bounds("background-row-98413").is_none(),
            "the list is shut"
        );
        click(cx, "background-link-call_1");
        assert!(
            cx.debug_bounds("background-row-98413").is_some(),
            "the call's window opens the list"
        );
        click(cx, "background-stop-98413");
        assert_eq!(stops(&thread), [Some("98413".to_owned())]);
    }

    /// A running command can be moved into the background where the
    /// thread's harness can do that (Claude Code's own Ctrl+B), and the
    /// move reaches the host; where it cannot, nothing offers it.
    #[gpui::test]
    fn a_running_call_moves_to_the_background_where_the_harness_can(cx: &mut TestAppContext) {
        let events = vec![
            AgentEvent::TurnStarted {
                turn_id: "t1".into(),
            },
            user_event("u1", "Build it."),
            AgentEvent::ItemStarted(ThreadItem {
                id: "call_2".into(),
                parent_item_id: None,
                content: ItemContent::CommandExecution {
                    command: "cargo build --release".into(),
                    output: String::new(),
                    exit_code: None,
                    status: ItemStatus::InProgress,
                },
            }),
        ];
        let mut thread = ScriptedThread::new(cx, events, 0, true);
        thread.status.provider = agent::ProviderKind::ClaudeCode;
        thread.set_running(true);
        let (_view, cx) = thread.open(cx);
        cx.update(|_, cx| cx.set_reduce_motion(true));
        thread.deliver(cx);
        draw_settled(cx);
        while thread.outgoing.try_recv().is_ok() {}
        click(cx, "move-to-background-call_2");
        let moved: Vec<_> = std::iter::from_fn(|| thread.outgoing.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    ClientPayload::Command(Command::MoveToBackground { item_id, .. }) => {
                        Some(item_id)
                    }
                    _ => None,
                },
            )
            .collect();
        assert_eq!(moved, ["call_2"]);

        thread.status.provider = agent::ProviderKind::Codex;
        thread.set_running(true);
        thread.deliver(cx);
        draw_settled(cx);
        assert!(cx.debug_bounds("move-to-background-call_2").is_none());
    }
}
