//! A pane is `[conversation | panel]`: the right panel docks beside the
//! conversation it belongs to, and covers the pane only where there is no
//! room for both or the reader expanded it. Neighbouring panes never move.

use super::*;

/// The side-by-side minimum: the panel's narrowest, and a pane's own beside it.
/// The panel's narrowest holds its content and, where the window draws its
/// own caption buttons, those buttons in its strip.
const LEAST: f32 = crate::material::PANEL_CONTENT_MIN_WIDTH
    + crate::window_caption::RIGHT_PANEL_CAPTION_WIDTH
    + PANE_MIN_WIDTH;

fn conversation(cx: &mut VisualTestContext) -> gpui::Bounds<Pixels> {
    cx.debug_bounds("pane-conversation")
        .expect("the conversation")
}

fn panel(cx: &mut VisualTestContext) -> gpui::Bounds<Pixels> {
    cx.debug_bounds("right-panel").expect("the panel")
}

/// Whether the panel `panel` casts a shadow on what is around it.
fn casts_shadow(panel: gpui::Bounds<Pixels>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, _| {
        let scale = window.scale_factor();
        crate::test_frames::painted_shadows(window)
            .iter()
            .any(|shadow| {
                let bounds = shadow.element_bounds;
                shadow.color.a > 0.
                    && (bounds.origin.x.0 / scale - f32::from(panel.left())).abs() < 1.
                    && (bounds.size.width.0 / scale - f32::from(panel.size.width)).abs() < 1.
            })
    })
}

/// The composer of the first pane holding the keyboard.
fn typing(shell: &Entity<AppShell>, cx: &mut VisualTestContext) -> gpui::FocusHandle {
    let input = shell.read_with(cx, |shell, cx| {
        shell
            .column
            .as_ref()
            .expect("attached")
            .chat(cx)
            .read(cx)
            .composer()
            .read(cx)
            .input_focus_handle(cx)
    });
    cx.update(|window, cx| input.focus(window, cx));
    draw(cx);
    input
}

fn open(store: &Entity<WorkspaceStore>, thread: &str, tab: RightTab, cx: &mut VisualTestContext) {
    store.update(cx, |store, cx| store.toggle_tab_panel_for(thread, tab, cx));
    crate::test_frames::draw_settled(cx);
}

/// A window that leaves one pane `width` wide, the sidebar beside it.
fn one_pane(width: f32, cx: &mut VisualTestContext) {
    resize(cx, SIDEBAR_WIDTH + width);
    crate::test_frames::draw_settled(cx);
}

/// A panel opens beside its conversation, the two sharing the pane edge to
/// edge at the width the reader chose. The composer keeps the keyboard, and
/// Escape, which means something else in a docked panel, does not close it.
#[gpui::test]
fn a_panel_docks_beside_the_conversation_and_leaves_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    one_pane(1345., cx);
    let input = typing(&shell, cx);
    let pane = pane_bounds(&shell, cx)[0];

    open(&store, "thread-1", RightTab::Plan, cx);

    let (conversation_at, panel_at) = (conversation(cx), panel(cx));
    assert_eq!(conversation_at.right(), panel_at.left());
    assert_eq!(
        (conversation_at.left(), panel_at.right()),
        (pane.left(), pane.right())
    );
    assert_eq!(panel_at.size.width, px(560.));
    assert_eq!(pane_bounds(&shell, cx)[0], pane, "the pane's slot stays");
    assert!(!casts_shadow(panel_at, cx), "a peer, not floating over it");
    cx.update(|window, _| assert!(input.is_focused(window), "the composer keeps it"));

    // Escape is the panel's own to use with the keyboard in it.
    cx.simulate_click(panel_at.center(), gpui::Modifiers::default());
    draw(cx);
    cx.update(|window, _| assert!(!input.is_focused(window), "the press takes it"));
    cx.simulate_keystrokes("escape");
    crate::test_frames::draw_settled(cx);
    assert_eq!(panel_of(&store, "thread-1", cx), Some(RightTab::Plan));
    assert_eq!(panel(cx), panel_at);
}

/// While what the panel leaves holds the widest column, the panel costs the
/// conversation only its gutter: on every frame of the glide the column keeps
/// its width and moves to the middle of what is left, the list keeps every
/// row's height, and the reader's place in it stays.
#[gpui::test]
fn a_body_that_holds_the_column_measures_no_row_again(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let turns = (0..40).flat_map(|turn| {
        [agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: format!("ask-{turn}"),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: format!("Question {turn}: {}", "how does this wrap? ".repeat(30)),
                attachments: vec![],
            },
        })]
    });
    let (shell, host, cx) = mount_thread_with(cx, turns.collect());
    let store = store_of(&shell, cx);
    resize(cx, 2200.);
    draw_until(cx, &host, "chat-composer", None);
    crate::test_frames::draw_settled(cx);
    // Rows fill the list: the last drawn says how wide the list is, and how
    // tall the row was measured.
    let last = (0..200)
        .rev()
        .map(|row| &*format!("timeline-row-{row}").leak())
        .find(|row| cx.debug_bounds(row).is_some())
        .expect("a row");
    let row = cx.debug_bounds(last).expect("the row");
    let column = cx.debug_bounds("composer-card").expect("the composer");
    assert_eq!(
        column.size.width,
        px(crate::material::CHAT_CONTENT_MAX_WIDTH)
    );
    let bubble = cx
        .debug_bounds("user-bubble-ask-39")
        .expect("the last question");

    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", RightTab::Plan, cx)
    });
    draw(cx);
    let mut frames = 0;
    while cx.update(|window, cx| window.simulate_next_frame(cx)) > 0 {
        frames += 1;
        assert!(frames < 120, "the glide lands");
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
        let card = cx.debug_bounds("composer-card").expect("the composer");
        assert_eq!(card.size.width, column.size.width, "frame {frames}");
        assert_eq!(cx.debug_bounds(last).map(|at| at.size), Some(row.size));
    }
    assert!(frames > 2, "the split glides");
    crate::test_frames::draw_settled(cx);

    let left = conversation(cx);
    assert_eq!(left.right(), panel(cx).left());
    assert!(left.size.width >= px(768.), "{left:?}");
    let card = cx.debug_bounds("composer-card").expect("the composer");
    assert!(
        (card.center().x - left.center().x).abs() < px(1.),
        "{card:?} centred in {left:?}"
    );
    assert!(card.center().x < column.center().x, "only re-centred");
    assert_eq!(cx.debug_bounds(last).map(|at| at.size), Some(row.size));
    let moved = cx
        .debug_bounds("user-bubble-ask-39")
        .expect("the last question");
    assert_eq!(moved.size, bubble.size, "wrapped as it was");
    assert_eq!(
        moved.right() - bubble.right(),
        card.right() - column.right(),
        "the rows move with the composer"
    );
}

/// In a pane too narrow for both, the panel covers that pane, as it is what
/// the reader sees there: it takes the keyboard, Escape closes it, and the
/// keyboard goes back. The conversation keeps the pane beneath, and the
/// neighbour is not touched by any of it.
#[gpui::test]
fn a_narrow_pane_is_covered_and_its_neighbour_never_moves(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    side_by_side(&shell, || serve_thread(&host, "thread-2"), cx);
    // Narrow enough for a panel of 320 to leave no room for a conversation.
    cx.update(|_, cx| crate::device::DevicePreferences::set_right_panel_width(320, cx));
    resize(cx, 1440.);
    crate::test_frames::draw_settled(cx);
    let panes = pane_bounds(&shell, cx);
    assert!(panes[0].size.width < px(LEAST), "{panes:?}");
    let input = typing(&shell, cx);

    open(&store, "thread-1", RightTab::Plan, cx);

    assert_eq!(panel(cx), panes[0]);
    assert!(casts_shadow(panes[0], cx), "it floats over the pane");
    assert_eq!(pane_bounds(&shell, cx), panes, "the neighbour stays");
    cx.update(|window, _| assert!(!input.is_focused(window), "the panel has the keyboard"));

    cx.simulate_keystrokes("escape");
    crate::test_frames::draw_settled(cx);
    assert_eq!(panel_of(&store, "thread-1", cx), None);
    assert_eq!(cx.debug_bounds("right-panel"), None);
    assert_eq!(pane_bounds(&shell, cx), panes, "the neighbour stays");
    cx.update(|window, _| assert!(input.is_focused(window), "it goes back"));
}

/// A panel covering the pane would lie over an agent opened in that pane's
/// chat, so opening the agent closes it, and the agent shows as the width
/// says; the same goes for one the reader expanded.
#[gpui::test]
fn an_agent_opened_from_a_covering_panel_closes_it(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_with_agent(cx);
    // Its agent runs, and what shows that rests only with motion reduced.
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let store = store_of(&shell, cx);
    for (width, expanded) in [(800., false), (2000., true)] {
        resize(cx, width);
        crate::test_frames::draw_settled(cx);
        open(&store, "thread-1", RightTab::Agents, cx);
        if expanded {
            store.update(cx, |store, cx| {
                store.toggle_diff_expanded_for(Some("thread-1"), cx)
            });
            crate::test_frames::draw_settled(cx);
        }
        open_agent_from_panel(&shell, &host, cx);
        crate::test_frames::draw_settled(cx);
        assert_eq!(panel_of(&store, "thread-1", cx), None, "{width}");
        assert_eq!(cx.debug_bounds("right-panel"), None, "{width}");
        assert!(cx.debug_bounds("chat-side-header").is_some(), "{width}");
        store.update(cx, |store, cx| store.set_side_thread("thread-1", None, cx));
        crate::test_frames::draw_settled(cx);
        if expanded {
            store.update(cx, |store, cx| {
                store.toggle_diff_expanded_for(Some("thread-1"), cx)
            });
        }
    }
}

/// Expanded by its own button, the panel covers the pane, floating over it,
/// and the conversation stays laid out as it was beneath, Preview's too;
/// Escape closes it there; restored, it is back at its width.
#[gpui::test]
fn an_expanded_panel_covers_the_pane_and_reflows_nothing(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    one_pane(1345., cx);
    let pane = pane_bounds(&shell, cx)[0];
    let mut tabs = vec![(RightTab::Plan, "diff-expand")];
    if cfg!(preview_backend) {
        tabs.push((RightTab::Preview, "preview-expand"));
    }
    for (tab, expand) in tabs {
        open(&store, "thread-1", tab, cx);
        let docked = (panel(cx), conversation(cx));
        let composer = cx.debug_bounds("composer-card").expect("the composer");

        press_control(expand, cx);
        crate::test_frames::draw_settled(cx);
        assert_eq!(panel(cx), pane, "{tab:?}");
        assert!(casts_shadow(pane, cx), "{tab:?}");
        assert_eq!(conversation(cx), docked.1, "{tab:?}");
        assert_eq!(cx.debug_bounds("composer-card"), Some(composer), "{tab:?}");

        press_control(expand, cx);
        crate::test_frames::draw_settled(cx);
        assert_eq!((panel(cx), conversation(cx)), docked, "{tab:?}");
    }
}

/// An expanded panel covers its pane, so Escape closes it, as it does in a
/// pane too narrow for both.
#[gpui::test]
fn escape_closes_an_expanded_panel(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    one_pane(1345., cx);
    open(&store, "thread-1", RightTab::Plan, cx);
    press_control("diff-expand", cx);
    crate::test_frames::draw_settled(cx);

    cx.simulate_keystrokes("escape");
    crate::test_frames::draw_settled(cx);
    assert_eq!(panel_of(&store, "thread-1", cx), None);
}

/// A pane narrowing into cover hands the panel the keyboard held in what it
/// now covers, so nothing is typed into what cannot be seen, and widening
/// back hands it to where it was. A pane without the keyboard takes nothing.
#[gpui::test]
fn a_pane_narrowing_into_cover_hands_the_panel_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    one_pane(LEAST, cx);
    let input = typing(&shell, cx);
    open(&store, "thread-1", RightTab::Plan, cx);
    cx.update(|window, _| assert!(input.is_focused(window), "docked, the composer keeps it"));

    one_pane(LEAST - 1., cx);
    cx.update(|window, _| assert!(!input.is_focused(window), "the composer is covered"));
    assert_eq!(keyboard_pane(&shell, cx), Some(0), "the panel has it");

    one_pane(LEAST, cx);
    cx.update(|window, _| assert!(input.is_focused(window), "it goes back"));

    store.update(cx, |store, cx| {
        store.close_right_panel_for(Some("thread-1"), cx)
    });
    crate::test_frames::draw_settled(cx);
    resize(cx, 2000.);
    crate::test_frames::draw_settled(cx);
    side_by_side(&shell, || serve_thread(&host, "thread-2"), cx);
    let input = typing(&shell, cx);
    open(&store, "thread-2", RightTab::Plan, cx);
    resize(cx, 1440.);
    crate::test_frames::draw_settled(cx);
    assert_eq!(panel(cx), pane_bounds(&shell, cx)[1], "it covers its pane");
    cx.update(|window, _| assert!(input.is_focused(window), "the other pane's stays"));
}

/// A page in a docked panel fills the panel's slot, and the panel slides to
/// it at its full width: the native page is not resized in flight.
#[cfg(preview_backend)]
#[gpui::test]
fn a_docked_page_fills_its_slot_and_keeps_its_size_as_it_slides(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    one_pane(1345., cx);
    store.update(cx, |store, cx| {
        store.toggle_tab_panel_for("thread-1", RightTab::Preview, cx)
    });
    render_preview_here(&host, "thread-1", cx);
    draw(cx);
    let mut widths = Vec::new();
    for _ in 0..60 {
        if cx.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
            break;
        }
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
        if let Some(body) = cx.debug_bounds("preview-body") {
            widths.push(body.size.width);
        }
    }
    crate::test_frames::draw_settled(cx);
    let (slot, body) = (
        panel(cx),
        cx.debug_bounds("preview-body").expect("the page"),
    );
    assert_eq!(slot.left(), conversation(cx).right());
    assert_eq!(slot.right(), pane_bounds(&shell, cx)[0].right());
    assert_eq!(
        (body.left(), body.right(), body.bottom()),
        (slot.left() + px(1.), slot.right(), slot.bottom()),
        "the page fills the panel, inside its hairline"
    );
    assert!(widths.len() > 2, "{widths:?}");
    assert!(
        widths.iter().all(|width| *width == body.size.width),
        "{widths:?}"
    );
}
