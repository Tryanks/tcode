//! The lead's timeline scrollbar stands left of whatever takes the slot right
//! of its column, in the gap the column keeps clear, and goes back to the
//! body's edge once nothing does.

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

/// With the rail or the agent's card in the slot, the track stands between
/// the column and it, the wheel over that gap moves the lead and the thumb
/// drags there, the grip's whole band staying the grip's; the agent's own
/// scrollbar stays on its card. The rail leaving takes the track back to the
/// body's edge over the rail's own glide, and the sheet leaves it there.
#[gpui::test]
fn the_leads_scrollbar_stands_left_of_the_slot(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    serve_finished_turns(&host, "thread-1", 60, false);
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(60)
    });
    let slack = px(0.5);

    for window in [WIDE, MIDDLE] {
        resize(cx, window);
        settle_frames(cx);
        let viewport = lead_viewport(&shell, cx);
        let rail = cx.debug_bounds("chat-rail").expect("the lead's rail");
        let column = cx
            .debug_bounds("composer-card")
            .expect("the lead's composer");
        let at = track(cx);
        let centred = (column.center().x - viewport.center().x).abs() < slack;
        assert_eq!(centred, window == WIDE, "{column:?} in {viewport:?}");
        assert!(
            (at.right() - rail.left()).abs() < slack && at.left() >= column.right() - slack,
            "{window}: {at:?} between {column:?} and {rail:?}"
        );

        let row = top_row(viewport.left(), rail.left(), cx);
        wheel(at.center(), 120., cx);
        assert!(
            top_row(viewport.left(), rail.left(), cx) != row,
            "{window}: the wheel over the gap moves the lead"
        );
    }

    let viewport = lead_viewport(&shell, cx);
    let rail = cx.debug_bounds("chat-rail").expect("the lead's rail");
    let column = cx
        .debug_bounds("composer-card")
        .expect("the lead's composer");
    let at = track(cx);
    let (row, _) = rows_in(viewport.left(), rail.left(), cx)[0];
    let thumb = point(at.right() - px(5.), at.bottom() - px(30.));
    drag(thumb, point(thumb.x, at.center().y), cx);
    assert!(
        rows_in(viewport.left(), rail.left(), cx)
            .iter()
            .all(|(index, _)| *index < row),
        "the thumb drags in the gap"
    );

    open_agent(&shell, &host, cx);
    let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
    let grip = cx.debug_bounds("chat-side-grip").expect("the card's grip");
    let band = grip.center().x - BAND / 2.;
    let viewport = lead_viewport(&shell, cx);
    let at = track(cx);
    let lead = lead_card(column, cx).expect("the lead's composer keeps its width");
    assert!(
        at.left() >= lead.right() - slack
            && at.right() <= band + slack
            && band + BAND <= card.left(),
        "{at:?} between {lead:?} and the grip's band at {band:?}, left of {card:?}"
    );
    let own = cx
        .debug_bounds("chat-side-track")
        .expect("the agent's track");
    assert!(
        (own.right() - card.right()).abs() < px(2.),
        "{own:?} at the edge of {card:?}"
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
    assert!(
        (track(cx).right() - (at.right() + dragged.left() - card.left())).abs() < slack,
        "the track follows the card's edge"
    );

    press_control("side-close", cx);
    settle_panes(cx);
    settle_frames(cx);
    let rail = cx
        .debug_bounds("chat-rail")
        .expect("the rail takes the slot");
    assert!(
        (track(cx).right() - rail.left()).abs() < slack,
        "{:?} back beside {rail:?}",
        track(cx)
    );

    resize(cx, NARROW);
    let viewport = lead_viewport(&shell, cx);
    let mut glide = vec![track(cx).right()];
    for _ in 0..30 {
        display_frame(cx);
        glide.push(track(cx).right());
    }
    assert!(
        glide.is_sorted()
            && glide
                .iter()
                .any(|at| *at > glide[0] && *at < viewport.right()),
        "the track glides to the body's edge at {:?}: {glide:?}",
        viewport.right()
    );
    assert!((*glide.last().unwrap() - viewport.right()).abs() < slack);
    assert_eq!(cx.debug_bounds("chat-rail"), None, "the rail has left");

    open_agent(&shell, &host, cx);
    assert_eq!(cx.debug_bounds("chat-side-card"), None, "on a sheet");
    assert!(cx.debug_bounds("chat-side-header").is_some(), "the sheet");
    assert!(
        (track(cx).right() - viewport.right()).abs() < slack,
        "the track stays at the body's edge under the sheet"
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
