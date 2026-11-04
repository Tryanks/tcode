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

thread_local! {
    /// When the test began, and how far the test clock has been advanced.
    static PACE: std::cell::Cell<(std::time::Instant, Duration)> =
        std::cell::Cell::new((std::time::Instant::now(), Duration::ZERO));
}

/// Lets `time` pass. gpui's touch recognizer reads the wall clock, Tcode's
/// motion the test clock, and in the app they are one clock: a finger that
/// paused has paused for both, or it would release into a fling no finger
/// gave. Wall time is kept from falling behind, as `test_frames` keeps it.
fn pass(cx: &mut VisualTestContext, time: Duration) {
    cx.executor().advance_clock(time);
    let (started, elapsed) = PACE.get();
    let elapsed = elapsed + time;
    PACE.set((started, elapsed));
    std::thread::sleep(elapsed.saturating_sub(started.elapsed()));
}

struct Phone {
    sidebar: gpui::Entity<SessionsSidebar>,
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
            false,
            cx,
        )
    });
    crate::store::tests::place_shown_threads(&store, cx);
    driver.wait_until(cx, &store, "sidebar baseline", |cx| {
        store.read_with(cx, |store, _| store.baseline_ready())
    });
    let navigation = cx.new(|_| WindowState::new(false).with_compact(true));
    let sidebar = cx.new(|cx| SessionsSidebar::new(store.clone(), navigation.clone(), cx));
    let (_, cx) =
        cx.add_window_view(|window, cx| gpui_base::Root::new(sidebar.clone(), window, cx));
    cx.update(|_, cx| cx.set_reduce_motion(false));
    cx.simulate_resize(size(px(WIDTH), px(852.)));
    draw_settled(cx);
    (
        Phone {
            sidebar,
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

    /// Display frames, the host answering between them, until `ready`.
    fn until(
        &mut self,
        cx: &mut VisualTestContext,
        what: &str,
        mut ready: impl FnMut(&Phone, &mut VisualTestContext) -> bool,
    ) {
        for _ in 0..300 {
            frame(cx);
            self.driver.step(cx, &self.store);
            if ready(self, cx) {
                draw(cx);
                return;
            }
        }
        panic!("waiting for {what}");
    }

    /// Everything sent so far has reached the host and come back.
    fn round_trip(&mut self, cx: &mut VisualTestContext) {
        let link = self.driver.link.clone();
        self.driver
            .await_with_events(cx, &self.store, link.query(tcode_protocol::Query::Ping))
            .unwrap();
        self.driver.step(cx, &self.store);
    }

    fn command(&mut self, cx: &mut VisualTestContext, command: Command) {
        let link = self.driver.link.clone();
        self.driver
            .await_with_events(cx, &self.store, link.command(command))
            .unwrap();
        self.driver.step(cx, &self.store);
    }

    fn undo(&self, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.undo_lifecycle(window, cx))
        });
    }

    fn close(mut self, cx: &mut VisualTestContext) {
        self.driver.shutdown(cx, &self.store, &self.host);
        let _ = std::fs::remove_dir_all(self.root);
    }
}

fn frame(cx: &mut VisualTestContext) {
    pass(cx, FRAME);
    cx.update(|window, cx| window.simulate_next_frame(cx));
    draw(cx);
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

/// The maintainer's report: the row an Undo brings back stayed in the
/// swipe's last frame. It comes back at rest, opening its room as any row
/// that arrives does.
#[gpui::test]
fn settling_then_undoing_brings_the_row_back_at_rest(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-undo", 3);
    let rest = phone.row(cx, 0);
    let below = phone.row(cx, 1).top();

    swipe_away(cx, &phone, 0);
    phone.until(cx, "the thread to settle", |phone, cx| phone.settled(cx, 0));
    phone.until(cx, "the row to go", |phone, cx| {
        phone.try_row(cx, 0).is_none() && phone.row(cx, 1).top() == rest.top()
    });
    draw_settled(cx);

    phone.undo(cx);
    phone.until(cx, "the thread to return", |phone, cx| {
        !phone.settled(cx, 0)
    });
    let mut opened = Vec::new();
    for _ in 0..24 {
        let row = phone.row(cx, 0);
        assert_eq!(row.left(), rest.left(), "it draws where a row at rest does");
        assert_eq!(row.size, rest.size);
        opened.push(phone.row(cx, 1).top());
        frame(cx);
    }
    assert!(
        opened.first().is_some_and(|top| *top < below) && opened.windows(2).all(|w| w[0] <= w[1]),
        "the row opens its room: {opened:?}"
    );
    assert_eq!(phone.row(cx, 1).top(), below);
    phone.close(cx);
}

/// A refresh that takes the row away mid-swipe ends the swipe: the finger
/// that was on it holds nothing, and the row comes back at rest.
fn refreshed_mid_swipe(cx: &mut TestAppContext, name: &str, lift_while_gone: bool) {
    let (mut phone, cx) = phone(cx, name, 3);
    let rest = phone.row(cx, 0);
    let mut finger = Finger::down(cx, 1, rest.center());
    finger.stroke(cx, -20., 4, FRAME);
    assert_eq!(phone.row(cx, 0).left(), rest.left() - px(80.));

    let session_id = "swipe-0".to_string();
    phone.command(
        cx,
        Command::ArchiveSession {
            session_id: session_id.clone(),
        },
    );
    phone.until(cx, "the row to go", |phone, cx| {
        phone.try_row(cx, 0).is_none()
    });
    draw_settled(cx);
    if lift_while_gone {
        finger.lift(cx);
    }
    phone.command(cx, Command::UnarchiveSession { session_id });
    phone.until(cx, "the row to return", |phone, cx| {
        phone.try_row(cx, 0).is_some()
    });
    assert_eq!(phone.row(cx, 0).left(), rest.left());
    if !lift_while_gone {
        finger.slide(cx, -20., 0., FRAME);
        assert_eq!(
            phone.row(cx, 0).left(),
            rest.left(),
            "the finger that was on it does not take it up again"
        );
        finger.lift(cx);
    }
    draw_settled(cx);
    assert_eq!(phone.row(cx, 0), rest);
    phone.close(cx);
}

#[gpui::test]
fn a_row_refreshed_away_returns_at_rest_after_the_finger_lifted(cx: &mut TestAppContext) {
    refreshed_mid_swipe(cx, "swipe-refresh-lifted", true);
}

#[gpui::test]
fn a_row_refreshed_away_returns_at_rest_under_the_same_finger(cx: &mut TestAppContext) {
    refreshed_mid_swipe(cx, "swipe-refresh-held", false);
}

/// A thread that began working as it was swiped is refused by the host,
/// which stays where it was: the row is back in the list, at rest.
#[gpui::test]
fn a_refused_settle_leaves_the_row_at_rest(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-refused", 3);
    let rest = phone.row(cx, 0);
    phone
        .store
        .update(cx, |store, cx| store.show_thread("swipe-0".into(), cx));
    phone.until(cx, "the thread to open", |phone, cx| {
        phone
            .store
            .read_with(cx, |store, _| !store.chat_loading_for(Some("swipe-0")))
    });
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
        phone
            .try_row(cx, 0)
            .is_some_and(|row| row.origin == rest.origin)
    });
    assert!(!phone.settled(cx, 0));
    phone.close(cx);
}

/// The first ten points decide. A scroll drifting sideways stays a scroll
/// whatever the finger does next, and a swipe drifting vertically stays a
/// swipe.
#[gpui::test]
fn a_pan_stays_on_the_axis_its_first_points_chose(cx: &mut TestAppContext) {
    let (phone, cx) = phone(cx, "swipe-axis", 40);
    let rest = phone.row(cx, 4);

    let mut scroll = Finger::down(cx, 1, rest.center());
    for _ in 0..3 {
        scroll.slide(cx, -3., -10., FRAME);
    }
    for _ in 0..4 {
        scroll.slide(cx, -30., -2., FRAME);
    }
    let scrolled = phone.row(cx, 4);
    assert!(scrolled.top() < rest.top(), "the list follows the scroll");
    assert_eq!(scrolled.left(), rest.left(), "the row stays put");
    pass(cx, ms(120));
    scroll.lift(cx);
    draw_settled(cx);
    assert_eq!(phone.row(cx, 4).left(), rest.left());

    let rest = phone.row(cx, 5);
    let mut swipe = Finger::down(cx, 2, rest.center());
    for _ in 0..3 {
        swipe.slide(cx, -10., 3., FRAME);
    }
    assert_eq!(phone.row(cx, 5).left(), rest.left() - px(30.));
    for _ in 0..4 {
        swipe.slide(cx, -2., -30., FRAME);
    }
    let swiped = phone.row(cx, 5);
    assert_eq!(swiped.left(), rest.left() - px(38.));
    assert_eq!(swiped.top(), rest.top(), "the list stays put");
    pass(cx, ms(120));
    swipe.lift(cx);
    phone.close(cx);
}

/// Every frame the row stands where the finger is, from the frame the pan
/// is known: the distance the recognizer waited for is not lost, and a
/// change of direction is followed at once.
#[gpui::test]
fn the_row_stands_under_the_finger_on_every_frame(cx: &mut TestAppContext) {
    let (phone, cx) = phone(cx, "swipe-track", 3);
    let rest = phone.row(cx, 0);
    let mut finger = Finger::down(cx, 1, rest.center());
    let mut travel = 0.;
    for (step, dx) in [-3., -3., -4., -6., -11., -17., -9., 6., 14., -22., -31.]
        .into_iter()
        .enumerate()
    {
        travel += dx;
        finger.slide(cx, dx, 0., ms(8 + 8 * (step as u64 % 2)));
        let expected = if step < 2 { px(0.) } else { px(travel) };
        assert_eq!(
            phone.row(cx, 0).left() - rest.left(),
            expected,
            "at step {step}"
        );
    }
    pass(cx, ms(120));
    finger.lift(cx);
    phone.close(cx);
}

/// Past the distance at which it acts the row gives ground grudgingly, and
/// so it does the way it has no action.
#[gpui::test]
fn past_the_action_and_the_wrong_way_the_row_resists(cx: &mut TestAppContext) {
    let (phone, cx) = phone(cx, "swipe-band", 3);
    let rest = phone.row(cx, 0);
    let action = px(WIDTH / 3.);

    let mut finger = Finger::down(cx, 1, rest.center());
    let mut last = px(0.);
    let mut gains = Vec::new();
    for step in 1..=14 {
        finger.slide(cx, -20., 0., FRAME);
        let shown = rest.left() - phone.row(cx, 0).left();
        if px(20. * step as f32) <= action {
            assert_eq!(shown, px(20. * step as f32));
        } else {
            gains.push(shown - last);
        }
        last = shown;
    }
    assert!(
        gains.windows(2).all(|pair| pair[1] <= pair[0])
            && gains[0] < px(20.)
            && gains.last() < gains.first()
            && *gains.last().unwrap() > px(0.),
        "each step past the action gains less: {gains:?}"
    );
    assert!(last < px(WIDTH), "the row never runs off under the finger");
    finger.stroke(cx, 20., 14, FRAME);
    assert_eq!(
        phone.row(cx, 0).left(),
        rest.left(),
        "the way back gives the same ground"
    );

    let mut pulled = Vec::new();
    for _ in 0..6 {
        finger.slide(cx, 20., 0., FRAME);
        pulled.push(phone.row(cx, 0).left() - rest.left());
    }
    assert!(
        pulled[0] > px(0.)
            && pulled[0] < px(20.)
            && pulled.windows(2).all(|pair| pair[1] > pair[0])
            && pulled[5] < px(60.),
        "the wrong way gives little: {pulled:?}"
    );
    pass(cx, ms(120));
    finger.lift(cx);
    draw_settled(cx);
    assert_eq!(phone.row(cx, 0), rest);
    assert!(!phone.settled(cx, 0));
    phone.close(cx);
}

/// A swipe that begins the wrong way is the same swipe: it comes back
/// through rest and on to the action.
#[gpui::test]
fn a_swipe_begun_the_wrong_way_can_still_act(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-reverse", 3);
    let rest = phone.row(cx, 0);
    let mut finger = Finger::down(cx, 1, rest.center());
    finger.stroke(cx, 12., 4, FRAME);
    finger.stroke(cx, -30., 10, FRAME);
    pass(cx, ms(120));
    finger.lift(cx);
    phone.until(cx, "the thread to settle", |phone, cx| phone.settled(cx, 0));
    phone.close(cx);
}

/// Where the finger was and how fast it was going both count: a short flick
/// acts, and a slow or quick pull back cancels even past the action.
#[gpui::test]
fn release_weighs_where_the_row_is_and_how_the_finger_moved(cx: &mut TestAppContext) {
    type Stroke = fn(&mut Finger, &mut VisualTestContext);
    let cases: [(&str, Stroke, bool); 7] = [
        (
            "a short quick flick",
            |finger, cx| finger.stroke(cx, -12., 6, ms(8)),
            true,
        ),
        (
            "a drag past the action, held still",
            |finger, cx| {
                finger.stroke(cx, -25., 8, FRAME);
                pass(cx, ms(120));
            },
            true,
        ),
        (
            "a slow short drag",
            |finger, cx| finger.stroke(cx, -3., 12, FRAME),
            false,
        ),
        (
            "a slow pull back past the action",
            |finger, cx| {
                finger.stroke(cx, -25., 8, FRAME);
                finger.stroke(cx, 3., 16, FRAME);
            },
            false,
        ),
        (
            "a quick pull back past the action",
            |finger, cx| {
                finger.stroke(cx, -25., 8, FRAME);
                finger.stroke(cx, 12., 5, ms(8));
            },
            false,
        ),
        (
            "a flick that stopped before the finger lifted",
            |finger, cx| {
                finger.stroke(cx, -12., 6, ms(8));
                pass(cx, ms(60));
            },
            false,
        ),
        (
            "a slow drag whose last report came early",
            |finger, cx| {
                finger.stroke(cx, -3., 12, FRAME);
                finger.slide(cx, -3., 0., ms(1));
            },
            false,
        ),
    ];
    // Below the undo notice a commit raises.
    const FIRST: usize = 6;
    let (mut phone, cx) = phone(cx, "swipe-release", FIRST + cases.len() + 1);
    for (case, (name, stroke, commits)) in cases.into_iter().enumerate() {
        let index = FIRST + case;
        let rest = phone.row(cx, index);
        let mut finger = Finger::down(cx, index as u64 + 1, rest.center());
        stroke(&mut finger, cx);
        finger.lift(cx);
        if commits {
            phone.until(cx, name, |phone, cx| phone.settled(cx, index));
            draw_settled(cx);
        } else {
            draw_settled(cx);
            phone.round_trip(cx);
            draw_settled(cx);
            assert!(!phone.settled(cx, index), "{name}");
            assert_eq!(phone.row(cx, index), rest, "{name} leaves the row at rest");
        }
    }
    phone.close(cx);
}

/// The row leaves at the speed the finger gave it, without a beat at the
/// release, and its room closes behind it without waiting for the host.
#[gpui::test]
fn a_commit_carries_the_row_off_and_closes_its_room_without_a_pause(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-commit", 3);
    let rest = phone.row(cx, 0);
    let mut finger = Finger::down(cx, 1, rest.center());
    finger.stroke(cx, -20., 8, ms(8));
    let released = rest.left() - phone.row(cx, 0).left();
    finger.lift(cx);

    frame(cx);
    let first = rest.left() - phone.row(cx, 0).left() - released;
    assert!(
        first >= px(30.),
        "at 2500pt/s a frame carries the row some 40pt: {first:?}"
    );

    let mut tops = vec![phone.row(cx, 1).top()];
    let mut lefts = Vec::new();
    for _ in 0..40 {
        if let Some(row) = phone.try_row(cx, 0) {
            lefts.push(row.left());
        }
        frame(cx);
        tops.push(phone.row(cx, 1).top());
    }
    assert!(
        lefts.windows(2).all(|pair| pair[1] <= pair[0]),
        "it only ever goes on: {lefts:?}"
    );
    assert!(
        tops.windows(2).all(|pair| pair[1] <= pair[0]),
        "the row below only rises: {tops:?}"
    );
    assert!(
        tops.windows(2).all(|pair| pair[0] - pair[1] < px(24.)),
        "and glides: {tops:?}"
    );
    assert_eq!(
        *tops.last().unwrap(),
        rest.top(),
        "with the host still to answer: {tops:?}"
    );

    phone.until(cx, "the thread to settle", |phone, cx| phone.settled(cx, 0));
    draw_settled(cx);
    assert_eq!(
        phone.row(cx, 1).top(),
        rest.top(),
        "its reply moves nothing"
    );
    phone.close(cx);
}

/// A row pulled back and let go carries on home at the speed it had, and
/// comes to rest with no more than a small bounce.
#[gpui::test]
fn a_cancelled_row_goes_home_at_the_speed_it_had(cx: &mut TestAppContext) {
    let (phone, cx) = phone(cx, "swipe-home", 3);
    let rest = phone.row(cx, 0);
    let mut finger = Finger::down(cx, 1, rest.center());
    finger.stroke(cx, -20., 5, FRAME);
    finger.stroke(cx, 10., 6, ms(8));
    let released = rest.left() - phone.row(cx, 0).left();
    finger.lift(cx);

    frame(cx);
    let first = released - (rest.left() - phone.row(cx, 0).left());
    assert!(
        first >= px(14.),
        "at 1250pt/s a frame carries the row some 20pt: {first:?}"
    );
    let mut lefts = Vec::new();
    for _ in 0..40 {
        lefts.push(phone.row(cx, 0).left() - rest.left());
        frame(cx);
    }
    assert!(
        lefts.iter().all(|left| *left <= px(6.)),
        "no more than a small bounce: {lefts:?}"
    );
    assert_eq!(phone.row(cx, 0), rest);
    phone.close(cx);
}

/// A finger put on a moving row stops it there, whether it was going home or
/// going off, and the row is the finger's from the first point: lifted
/// without moving it takes no tap, and pulled back it cancels the settle.
#[gpui::test]
fn a_touch_catches_a_row_on_its_way_home(cx: &mut TestAppContext) {
    let (phone, cx) = phone(cx, "swipe-catch-home", 3);
    let rest = phone.row(cx, 0);
    let mut finger = Finger::down(cx, 1, rest.center());
    finger.stroke(cx, -15., 6, FRAME);
    pass(cx, ms(120));
    finger.lift(cx);
    frame(cx);
    frame(cx);

    let held = phone.row(cx, 0).left();
    assert!(held < rest.left() && held > rest.left() - px(90.));
    let at = phone.row(cx, 0).center();
    let mut finger = Finger::down(cx, 2, at);
    for _ in 0..4 {
        frame(cx);
        assert_eq!(phone.row(cx, 0).left(), held, "the touch holds it");
    }
    finger.slide(cx, -3., 0., FRAME);
    assert_eq!(
        phone.row(cx, 0).left(),
        held - px(3.),
        "from the first point"
    );
    finger.slide(cx, 2., 0., FRAME);
    assert_eq!(phone.row(cx, 0).left(), held - px(1.));
    pass(cx, ms(120));
    finger.lift(cx);
    draw_settled(cx);
    assert_eq!(phone.row(cx, 0), rest);
    assert!(
        phone
            .store
            .read_with(cx, |store, _| store.open_threads().is_empty()),
        "the catch was not a tap"
    );
    phone.close(cx);
}

#[gpui::test]
fn a_touch_catches_a_row_on_its_way_off(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-catch-off", 3);
    let rest = phone.row(cx, 0);
    swipe_away(cx, &phone, 0);
    frame(cx);

    let held = phone.row(cx, 0).left();
    assert!(held < rest.left() - px(WIDTH / 3.));
    let at = point(rest.center().x, rest.center().y);
    let mut finger = Finger::down(cx, 2, at);
    for _ in 0..4 {
        frame(cx);
        assert_eq!(phone.row(cx, 0).left(), held, "the touch holds it");
    }
    finger.stroke(cx, 20., 5, FRAME);
    assert_eq!(phone.row(cx, 0).left(), held + px(100.));
    pass(cx, ms(120));
    finger.lift(cx);
    draw_settled(cx);
    phone.round_trip(cx);
    draw_settled(cx);
    assert_eq!(phone.row(cx, 0), rest);
    assert!(
        !phone.settled(cx, 0),
        "pulled back in time, it is not settled"
    );
    phone.close(cx);
}

/// A swipe from the screen's left edge inward is navigation's wherever it
/// starts, and a row there still swipes the other way.
#[gpui::test]
fn the_screen_edge_keeps_only_the_inward_swipe(cx: &mut TestAppContext) {
    let (phone, cx) = phone(cx, "swipe-edge", 3);
    let rest = phone.row(cx, 0);
    let edge = point(rest.left() + px(12.), rest.center().y);

    let mut inward = Finger::down(cx, 1, edge);
    inward.stroke(cx, 20., 6, FRAME);
    assert_eq!(phone.row(cx, 0), rest, "the row leaves it to navigation");
    pass(cx, ms(120));
    inward.lift(cx);
    draw_settled(cx);

    let at = point(rest.left() + px(19.), rest.center().y);
    let mut outward = Finger::down(cx, 2, at);
    outward.stroke(cx, -3., 4, FRAME);
    assert_eq!(phone.row(cx, 0).left(), rest.left() - px(12.));
    pass(cx, ms(120));
    outward.lift(cx);
    draw_settled(cx);
    assert_eq!(phone.row(cx, 0), rest);
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
    let shelved = phone.row(cx, 3);
    assert!(shelved.top() > shelf.top(), "the shelf has opened");

    swipe_away(cx, &phone, 3);
    let mut seen = false;
    for _ in 0..30 {
        frame(cx);
        seen |= phone.try_row(cx, 3).is_none();
    }
    assert!(seen, "the row goes before the host answers");
    assert!(phone.try_row(cx, 2).is_some(), "the others stay");
    phone.until(cx, "the thread to return", |phone, cx| {
        !phone.settled(cx, 3)
    });
    draw_settled(cx);
    assert_eq!(phone.row(cx, 3).left(), shelved.left());
    assert_eq!(phone.row(cx, 3).size, shelved.size);
    phone.close(cx);
}

/// With motion reduced the finger still moves the row, but its release
/// changes the list at once, with nothing left to spring.
#[gpui::test]
fn reduced_motion_lets_go_without_a_spring(cx: &mut TestAppContext) {
    let (mut phone, cx) = phone(cx, "swipe-reduced", 3);
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let rest = phone.row(cx, 0);

    let mut finger = Finger::down(cx, 1, rest.center());
    finger.stroke(cx, -10., 4, FRAME);
    assert_eq!(phone.row(cx, 0).left(), rest.left() - px(40.));
    pass(cx, ms(120));
    finger.lift(cx);
    assert_eq!(phone.row(cx, 0), rest, "cancelled, it is home at once");

    swipe_away(cx, &phone, 0);
    draw(cx);
    assert!(
        phone.try_row(cx, 0).is_none(),
        "committed, it is gone at once"
    );
    assert_eq!(phone.row(cx, 1).top(), rest.top());
    phone.until(cx, "the thread to settle", |phone, cx| phone.settled(cx, 0));
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
