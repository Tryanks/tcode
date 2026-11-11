//! The swipe as a phone drives it: raw touches a display frame apart at phone
//! geometry, read back from where the rows are drawn. The host answers only
//! when a test pumps it, which is what a slow connection looks like.

use std::time::Duration;

use gpui::{
    Bounds, PlatformInput, Point, TestAppContext, TouchEvent, TouchId, TouchPhase,
    VisualTestContext, point, size,
};
use tcode_core::project::Project;
use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
use tcode_services::store::SessionStore;

use super::*;
use crate::store::tests::LocalHostDriver;
use crate::test_frames::{draw, draw_settled};

const FRAME: Duration = Duration::from_millis(16);
const WIDTH: f32 = 393.;

/// Lets `time` pass on the executor's clock, which a swipe reads. gpui's
/// touch recognizer reads the wall clock, for which no time passes: a finger
/// lifted after a pause is still moving to it, and its release a fling,
/// which a row catches as it catches every released pan
/// (`SessionsSidebar::catch_row_fling`).
fn pass(cx: &mut VisualTestContext, time: Duration) {
    cx.executor().advance_clock(time);
}

struct Phone {
    store: gpui::Entity<WorkspaceStore>,
    driver: LocalHostDriver,
    host: SpawnedHost,
    root: std::path::PathBuf,
}

fn phone<'a>(
    cx: &'a mut TestAppContext,
    name: &str,
    threads: usize,
) -> (Phone, &'a mut VisualTestContext) {
    cx.update(crate::theme::init);
    let root = crate::store::tests::scratch_root(name);
    let host = spawn_host(
        SessionStore::open_at(root.clone()).unwrap(),
        HostServices::default(),
    )
    .unwrap();
    let project = Project::from_root(root.clone());
    let sessions: Vec<_> = (0..threads)
        .map(|index| {
            let mut meta = SessionMeta::new(agent::ProviderKind::Codex, root.clone(), None);
            meta.id = format!("swipe-{index}");
            meta.title = meta.id.clone();
            meta.project_id = Some(project.id.clone());
            meta.updated_at = 1000 - index as u64;
            meta.created_at = meta.updated_at;
            meta
        })
        .collect();
    smol::block_on(host.update_state_for_test(move |state, _| {
        state.projects = vec![project];
        state.sessions = sessions.into();
    }))
    .unwrap();
    let mut driver = LocalHostDriver::new(&host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    crate::store::tests::place_shown_threads(&store, cx);
    driver.wait_until(cx, "sidebar baseline", |cx| {
        store.read_with(cx, |store, _| store.baseline_ready())
    });
    let navigation = cx.new(|_| WindowState::default().with_compact(true));
    let sidebar = cx.new(|cx| SessionsSidebar::new(vec![store.clone()], navigation.clone(), cx));
    let (_, cx) = cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar, window, cx));
    cx.update(|_, cx| cx.set_reduce_motion(false));
    cx.simulate_resize(size(px(WIDTH), px(852.)));
    draw_settled(cx);
    (
        Phone {
            store,
            driver,
            host,
            root,
        },
        cx,
    )
}

impl Phone {
    fn row(&self, cx: &mut VisualTestContext, index: usize) -> Bounds<Pixels> {
        self.try_row(cx, index)
            .unwrap_or_else(|| panic!("row {index} is drawn"))
    }

    fn try_row(&self, cx: &mut VisualTestContext, index: usize) -> Option<Bounds<Pixels>> {
        cx.debug_bounds(format!("compact-row-swipe-{index}").leak())
    }

    fn settled(&self, cx: &VisualTestContext, index: usize) -> bool {
        self.store.read_with(cx, |store, _| {
            store
                .thread_meta(&format!("swipe-{index}"))
                .is_some_and(|meta| meta.is_settled())
        })
    }

    /// Display frames, the host answering between them, until `ready`. Once
    /// what the host said is drawn and the window asks for no frame, only
    /// the host can move it on, so the test waits for the host.
    fn until(
        &mut self,
        cx: &mut VisualTestContext,
        what: &str,
        mut ready: impl FnMut(&Phone, &mut VisualTestContext) -> bool,
    ) {
        loop {
            self.driver.step(cx);
            let asked = frame(cx);
            if ready(self, cx) {
                draw(cx);
                return;
            }
            if !asked && !frame(cx) {
                if ready(self, cx) {
                    draw(cx);
                    return;
                }
                self.driver.wait_for_host(what);
            }
        }
    }

    /// Everything sent so far has reached the host and come back.
    fn round_trip(&mut self, cx: &mut VisualTestContext) {
        let link = self.driver.link.clone();
        self.driver
            .await_with_events(cx, link.query(tcode_protocol::Query::Ping))
            .unwrap();
        self.driver.step(cx);
    }

    fn command(&mut self, cx: &mut VisualTestContext, command: Command) {
        let link = self.driver.link.clone();
        self.driver
            .await_with_events(cx, link.command(command))
            .unwrap();
        self.driver.step(cx);
    }

    /// `swipe-0` opened, so that the host runs turns for it.
    fn open_first(&mut self, cx: &mut VisualTestContext) {
        self.store
            .update(cx, |store, cx| store.show_thread("swipe-0".into(), cx));
        self.until(cx, "the thread to open", |phone, cx| {
            phone
                .store
                .read_with(cx, |store, cx| !store.chat_loading_for(Some("swipe-0"), cx))
        });
    }

    /// `swipe-0`'s agent starts a turn, or ends it, as its provider reports
    /// it, and the list hears of it.
    fn turn(&mut self, cx: &mut VisualTestContext, running: bool) {
        let event = if running {
            agent::AgentEvent::TurnStarted {
                turn_id: "busy".into(),
            }
        } else {
            agent::AgentEvent::TurnCompleted {
                turn_id: "busy".into(),
                status: agent::TurnStatus::Completed,
                usage: None,
            }
        };
        smol::block_on(self.host.update_state_for_test(move |state, cx| {
            state.provider_event_for_test("swipe-0", event, cx);
        }))
        .unwrap();
        self.until(cx, "the turn to reach the list", |phone, cx| {
            phone
                .store
                .read_with(cx, |store, _| store.turn_running_for("swipe-0"))
                == running
        });
    }

    /// What `swipe-0`'s swiped row has uncovered, by its words, once it is
    /// the only thing there.
    fn uncovers(&mut self, cx: &mut VisualTestContext, shown: &str, not: &str) {
        let what = format!("the row to show {shown}");
        let at = |label: &str| &*format!("row-swipe-shows-{label}-swipe-0").leak();
        let (shown, not) = (at(shown), at(not));
        self.until(cx, &what, |_, cx| {
            cx.debug_bounds(shown).is_some() && cx.debug_bounds(not).is_none()
        });
    }

    fn close(mut self, cx: &mut VisualTestContext) {
        self.driver.shutdown(cx, &self.host);
        let _ = std::fs::remove_dir_all(self.root);
    }
}

/// A display frame, and whether the window had asked for it.
fn frame(cx: &mut VisualTestContext) -> bool {
    pass(cx, FRAME);
    let asked = cx.update(|window, cx| window.simulate_next_frame(cx)) > 0;
    draw(cx);
    asked
}

#[derive(Clone, Copy)]
struct Finger {
    id: u64,
    at: Point<Pixels>,
}

impl Finger {
    fn down(cx: &mut VisualTestContext, id: u64, at: Point<Pixels>) -> Self {
        let finger = Self { id, at };
        finger.send(cx, TouchPhase::Started);
        finger
    }

    fn send(self, cx: &mut VisualTestContext, phase: TouchPhase) {
        cx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::Touch(TouchEvent {
                    id: TouchId(self.id),
                    phase,
                    position: self.at,
                    predicted_position: None,
                    force: None,
                }),
                cx,
            );
        });
    }

    /// One report from the panel, `every` after the last, drawn at once.
    fn slide(&mut self, cx: &mut VisualTestContext, dx: f32, dy: f32, every: Duration) {
        pass(cx, every);
        self.at += point(px(dx), px(dy));
        self.send(cx, TouchPhase::Moved);
        draw(cx);
    }

    fn stroke(&mut self, cx: &mut VisualTestContext, dx: f32, steps: usize, every: Duration) {
        for _ in 0..steps {
            self.slide(cx, dx, 0., every);
        }
    }

    fn lift(self, cx: &mut VisualTestContext) {
        self.send(cx, TouchPhase::Ended);
        draw(cx);
    }
}

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

/// A swipe that commits by anyone's rule: far past the action, finger still.
fn swipe_away(cx: &mut VisualTestContext, phone: &Phone, index: usize) {
    let at = phone.row(cx, index).center();
    let mut finger = Finger::down(cx, 1, at);
    finger.stroke(cx, -30., 10, FRAME);
    pass(cx, ms(120));
    finger.lift(cx);
}

/// The release does what the row shows as the finger lifts, and what it
/// shows follows the thread under the finger: a turn that ends lets the row
/// settle, and one that starts takes that away.
#[gpui::test]
fn a_row_does_what_its_thread_lets_it_when_the_finger_lifts(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-turns", 3);
    let rest = phone.row(cx, 0);
    phone.open_first(cx);

    let mut finger = Finger::down(cx, 1, rest.center());
    finger.stroke(cx, -25., 8, FRAME);
    phone.uncovers(cx, "Settle", "Working");
    phone.turn(cx, true);
    phone.uncovers(cx, "Working", "Settle");
    finger.lift(cx);
    phone.round_trip(cx);
    assert!(phone.try_row(cx, 0).is_some(), "the row stays in the list");
    assert!(
        !phone.settled(cx, 0),
        "a turn begun under it leaves it active"
    );

    let mut finger = Finger::down(cx, 2, rest.center());
    finger.stroke(cx, -25., 8, FRAME);
    phone.uncovers(cx, "Working", "Settle");
    phone.turn(cx, false);
    phone.uncovers(cx, "Settle", "Working");
    finger.lift(cx);
    phone.until(cx, "the thread to settle", |phone, cx| phone.settled(cx, 0));
    phone.close(cx);
}

/// A thread that began working as it was swiped is refused by the host,
/// which stays where it was: the row is back in the list, at rest.
#[gpui::test]
fn a_refused_settle_returns_the_row_and_leaves_the_thread_active(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-refused", 3);
    phone.open_first(cx);
    // The host hears of the turn before the store does.
    smol::block_on(phone.host.update_state_for_test(|state, cx| {
        let event = agent::AgentEvent::TurnStarted {
            turn_id: "busy".into(),
        };
        state.provider_event_for_test("swipe-0", event, cx);
    }))
    .unwrap();

    swipe_away(cx, &phone, 0);
    draw_settled(cx);
    phone.until(cx, "the row to return", |phone, cx| {
        phone.try_row(cx, 0).is_some()
    });
    assert!(!phone.settled(cx, 0));
    phone.close(cx);
}

/// A sideways pan no row takes moves nothing, so it must leave nothing
/// behind either: gpui flings it all the same, and the next tap would go on
/// catching that.
#[gpui::test]
fn a_sideways_pan_no_row_takes_does_not_swallow_the_next_tap(cx: &mut TestAppContext) {
    let (phone, cx) = phone(cx, "swipe-idle", 3);
    let rest = phone.row(cx, 0);
    let edge = point(rest.left() + px(12.), rest.center().y);
    let mut inward = Finger::down(cx, 1, edge);
    inward.stroke(cx, 16., 6, ms(8));
    inward.lift(cx);
    frame(cx);

    let tap = phone.row(cx, 1).center();
    let finger = Finger::down(cx, 2, tap);
    finger.send(cx, TouchPhase::Ended);
    draw(cx);
    assert_eq!(
        phone.store.read_with(cx, |store, _| store.open_threads()),
        ["swipe-1"]
    );
    phone.close(cx);
}

/// A settled row swipes the same way, out of an open shelf and back into the
/// active threads, and the shelf it leaves keeps its folds in step.
#[gpui::test]
fn a_settled_row_swipes_back_to_active(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-unsettle", 4);
    let session_id = "swipe-3".to_string();
    phone.command(cx, Command::SettleSession { session_id });
    phone.until(cx, "the thread to settle", |phone, cx| {
        phone.settled(cx, 3) && phone.try_row(cx, 3).is_none()
    });
    draw_settled(cx);
    let shelf = cx
        .debug_bounds("settled-recent")
        .expect("the settled shelf");
    let tap = Finger::down(cx, 1, shelf.center());
    tap.send(cx, TouchPhase::Ended);
    draw_settled(cx);
    assert!(phone.try_row(cx, 3).is_some(), "the shelf has opened");

    swipe_away(cx, &phone, 3);
    phone.until(cx, "the thread to return", |phone, cx| {
        !phone.settled(cx, 3)
    });
    draw_settled(cx);
    assert!(phone.try_row(cx, 2).is_some(), "the others stay");
    assert!(
        phone.try_row(cx, 3).is_some(),
        "it is among the active threads"
    );
    phone.close(cx);
}

#[gpui::test]
fn a_swiped_row_settles_and_the_next_tap_still_opens(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-tap", 3);
    let row = phone.row(cx, 0);
    let mut finger = Finger::down(cx, 1, row.center());
    finger.stroke(cx, -40., 6, FRAME);
    finger.lift(cx);
    phone.until(cx, "the thread to settle", |phone, cx| phone.settled(cx, 0));

    // The last row's lower edge, clear of the undo notice the settle raised,
    // which is still rising into place.
    let next = phone.row(cx, 2);
    let tap = point(next.center().x, next.bottom() - px(8.));
    let finger = Finger::down(cx, 2, tap);
    finger.send(cx, TouchPhase::Ended);
    draw(cx);
    assert_eq!(
        phone.store.read_with(cx, |store, _| store.open_threads()),
        ["swipe-2"],
        "the released swipe's fling does not swallow the next tap"
    );
    phone.close(cx);
}
