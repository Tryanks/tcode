//! The lead's timeline scrollbar stands just right of its content column,
//! wherever the column is: what takes the slot beside the column moves it
//! only by moving the column.

use gpui::point;

use super::*;

/// A window whose pane is too narrow for the rail's column to stay centred,
/// but holds the agent's card; one whose pane has the rail in the gutter of
/// the centred column; and one with no room for either.
const MIDDLE: f32 = 1400.;
const WIDE: f32 = 1800.;
const NARROW: f32 = 1100.;

/// How wide gpui-base's resize band is, which the grip is centred in.
const BAND: Pixels = px(9.);

fn track(cx: &mut VisualTestContext) -> gpui::Bounds<Pixels> {
    cx.debug_bounds("chat-track").expect("the lead's track")
}

/// The lead's column, as its composer's card spans it.
fn column(cx: &mut VisualTestContext) -> gpui::Bounds<Pixels> {
    cx.debug_bounds("composer-card")
        .expect("the lead's composer")
}

/// The track stands against the column's right edge.
fn hugs(at: gpui::Bounds<Pixels>, column: gpui::Bounds<Pixels>, when: &str) {
    assert!(
        (at.left() - column.right()).abs() < px(0.5),
        "{when}: {at:?} against {column:?}"
    );
}

fn top_row(left: Pixels, right: Pixels, cx: &mut VisualTestContext) -> gpui::Bounds<Pixels> {
    rows_in(left, right, cx).last().expect("the lead's rows").1
}

fn drag(from: gpui::Point<Pixels>, to: gpui::Point<Pixels>, cx: &mut VisualTestContext) {
    let none = gpui::Modifiers::default();
    cx.simulate_mouse_move(from, None, none);
    draw(cx);
    cx.simulate_mouse_down(from, MouseButton::Left, none);
    draw(cx);
    for step in [from + (to - from) / 2., to] {
        cx.simulate_mouse_move(step, Some(MouseButton::Left), none);
        draw(cx);
    }
    cx.simulate_mouse_up(to, MouseButton::Left, none);
    settle_frames(cx);
}

/// `id` starts a turn and runs a command in it.
fn serve_command_started(host: &MountedShell, id: &str) {
    let events = [
        agent::AgentEvent::TurnStarted {
            turn_id: "turn-build".into(),
        },
        agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: "build".into(),
            parent_item_id: None,
            content: agent::ItemContent::CommandExecution {
                command: "cargo build".into(),
                output: String::new(),
                exit_code: None,
                status: agent::ItemStatus::InProgress,
            },
        }),
    ];
    send_events(
        host,
        events
            .into_iter()
            .map(|event| {
                (
                    Topic::SessionEvents {
                        session_id: id.into(),
                    },
                    ServerEvent::SessionEvent(event.into()),
                )
            })
            .collect(),
    );
}

/// Opens `agent-1` in the lead's slot, or on a sheet where it has no room.
fn open_agent(shell: &Entity<AppShell>, host: &MountedShell, cx: &mut VisualTestContext) {
    let store = store_of(shell, cx);
    serve_finished_turns(host, "agent-1", 30, false);
    serve_subagent_status(host, "agent-1");
    cx.update(|window, cx| {
        crate::agents_panel::open_agent(
            &store,
            "agent-1".into(),
            crate::shell::side_host_for("thread-1", cx),
            None,
            window,
            cx,
        )
    });
    draw(cx);
    await_restore_update(shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-1"), cx) && store.timeline_settled_for("agent-1")
    });
    settle_panes(cx);
    settle_frames(cx);
}

/// With the rail empty the track stands against the column's right edge,
/// and a command bringing the rail in beside the centred column leaves it
/// there on every frame; where the column moves clear of the rail or of an
/// agent's card, the track moves with it, in the gap left of the slot, and
/// never over the grip's band. The wheel over the track and right of it
/// moves the lead, and the thumb drags where the track now is; the agent's
/// own scrollbar stays on its card. The rail leaving glides the column back,
/// the track with it, and the sheet leaves it at the column.
#[gpui::test]
fn the_leads_scrollbar_stands_at_its_columns_edge_whatever_takes_the_slot(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    serve_finished_turns(&host, "thread-1", 60, false);
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(60)
    });
    let slack = px(0.5);

    resize(cx, WIDE);
    settle_frames(cx);
    assert_eq!(cx.debug_bounds("chat-rail"), None, "the rail is empty");
    let at = track(cx);
    hugs(at, column(cx), "with the rail empty");

    serve_command_started(&host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(
            Some("thread-1"),
            |timeline| timeline.entries.iter().any(|entry| entry.id == "build"),
            cx,
        ) == Some(true)
    });
    for frame in 0..30 {
        assert_eq!(track(cx), at, "frame {frame} of the rail coming in");
        display_frame(cx);
    }
    let rail = cx
        .debug_bounds("chat-rail")
        .expect("the command's card brings the rail");
    assert_eq!(track(cx), at, "with the rail in");
    hugs(at, column(cx), "with the rail in");
    assert!(at.right() <= rail.left(), "{at:?} left of {rail:?}");

    let viewport = lead_viewport(&shell, cx);
    for (over, x) in [("track", at.center().x), ("gutter", at.right() + px(20.))] {
        let row = top_row(viewport.left(), rail.left(), cx);
        wheel(point(x, at.center().y), 120., cx);
        assert!(
            top_row(viewport.left(), rail.left(), cx) != row,
            "the wheel over the {over} moves the lead"
        );
    }

    resize(cx, MIDDLE);
    settle_frames(cx);
    let viewport = lead_viewport(&shell, cx);
    let rail = cx.debug_bounds("chat-rail").expect("the lead's rail");
    let beside = column(cx);
    let at = track(cx);
    assert!(
        beside.center().x < viewport.center().x - slack,
        "{beside:?} moves clear of {rail:?} in {viewport:?}"
    );
    hugs(at, beside, "beside the rail");
    assert!(at.right() <= rail.left() + slack, "{at:?} left of {rail:?}");
    let (row, _) = rows_in(viewport.left(), rail.left(), cx)[0];
    let thumb = point(at.right() - px(5.), at.bottom() - px(30.));
    drag(thumb, point(thumb.x, at.center().y), cx);
    assert!(
        rows_in(viewport.left(), rail.left(), cx)
            .iter()
            .all(|(index, _)| *index < row),
        "the thumb drags where the track is"
    );

    send_events(&host, vec![lead_with_agent_index("thread-1")]);
    await_restore_update(&shell, cx, |store, _| {
        store.thread_meta("agent-1").is_some()
    });
    open_agent(&shell, &host, cx);
    let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
    let grip = cx.debug_bounds("chat-side-grip").expect("the card's grip");
    let band = grip.center().x - BAND / 2.;
    let viewport = lead_viewport(&shell, cx);
    let at = track(cx);
    let lead = lead_card(beside, cx).expect("the lead's composer keeps its width");
    hugs(at, lead, "beside the agent's card");
    assert!(
        at.right() <= band + slack && band + BAND <= card.left(),
        "{at:?} left of the grip's band at {band:?}, left of {card:?}"
    );
    let own = cx
        .debug_bounds("chat-side-track")
        .expect("the agent's track");
    assert!(
        own.left() >= card.left() && own.right() <= card.right(),
        "{own:?} on {card:?}"
    );

    let row = top_row(viewport.left(), card.left(), cx);
    wheel(grip.center(), 120., cx);
    assert!(
        top_row(viewport.left(), card.left(), cx) != row,
        "the wheel over the grip moves the lead"
    );

    // The pixel either side of where the two meet.
    let row = top_row(viewport.left(), card.left(), cx);
    let y = at.bottom() - px(30.);
    let on_track = point(at.right() - px(1.), y);
    drag(on_track, on_track - point(px(20.), px(0.)), cx);
    assert_eq!(
        cx.debug_bounds("chat-side-card"),
        Some(card),
        "over the track, the card's edge stays"
    );
    assert!(
        top_row(viewport.left(), card.left(), cx) != row,
        "over the track, the track takes the press"
    );
    let on_band = point(band + px(1.), y);
    drag(on_band, on_band + point(px(20.), px(0.)), cx);
    let dragged = cx.debug_bounds("chat-side-card").expect("the agent's card");
    assert!(
        dragged.left() > card.left() + px(10.),
        "over the band, {dragged:?} narrows from {card:?}"
    );
    let lead = lead_card(beside, cx).expect("the lead's composer keeps its width");
    hugs(track(cx), lead, "with the card narrowed");

    press_control("side-close", cx);
    settle_panes(cx);
    settle_frames(cx);
    let rail = cx
        .debug_bounds("chat-rail")
        .expect("the rail takes the slot");
    hugs(track(cx), column(cx), "with the rail back");
    assert!(track(cx).right() <= rail.left() + slack);

    resize(cx, NARROW);
    let mut glide = vec![track(cx).left()];
    for frame in 0..30 {
        display_frame(cx);
        let at = track(cx);
        hugs(
            at,
            column(cx),
            &format!("frame {frame} of the rail leaving"),
        );
        glide.push(at.left());
    }
    assert!(
        glide.is_sorted()
            && glide
                .iter()
                .any(|at| *at > glide[0] && *at < *glide.last().unwrap()),
        "the track glides with the column: {glide:?}"
    );
    assert_eq!(cx.debug_bounds("chat-rail"), None, "the rail has left");
    let at = track(cx);
    hugs(at, column(cx), "with nothing in the slot");

    open_agent(&shell, &host, cx);
    assert_eq!(cx.debug_bounds("chat-side-card"), None, "on a sheet");
    assert!(cx.debug_bounds("chat-side-header").is_some(), "the sheet");
    assert_eq!(
        track(cx),
        at,
        "the track stays at the column under the sheet"
    );
}

/// The header has no divider, so the eye measures the room under its title
/// to the first thing below: the timeline, its track, and what takes the slot
/// all start at the header's bottom edge, the agent's sheet included.
#[gpui::test]
fn the_body_starts_at_the_headers_bottom_edge(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    serve_finished_turns(&host, "thread-1", 60, false);
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(60)
    });
    let under_header = |top: Pixels, what: &str, cx: &mut VisualTestContext| {
        let header = cx.debug_bounds("chat-header-scroll").expect("the header");
        assert_eq!(top, header.bottom(), "{what}");
    };

    resize(cx, WIDE);
    settle_frames(cx);
    let viewport = lead_viewport(&shell, cx);
    under_header(viewport.top(), "the timeline", cx);
    under_header(track(cx).top(), "the track", cx);
    let rail = cx.debug_bounds("chat-rail").expect("the lead's rail");
    under_header(rail.top(), "the rail", cx);

    open_agent(&shell, &host, cx);
    let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
    under_header(card.top(), "the agent's card", cx);

    press_control("side-close", cx);
    settle_panes(cx);
    settle_frames(cx);
    resize(cx, NARROW);
    settle_frames(cx);
    open_agent(&shell, &host, cx);
    let sheet = cx
        .debug_bounds("chat-side-sheet")
        .expect("the agent's sheet");
    under_header(sheet.top(), "the agent's sheet", cx);
}
