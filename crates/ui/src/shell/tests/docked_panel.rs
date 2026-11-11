//! A pane is `[conversation | panel]`: the right panel docks beside the
//! conversation it belongs to, and covers the pane only where there is no
//! room for both or the reader expanded it.

use super::*;

/// The side-by-side minimum: the panel's narrowest, and a pane's own beside it.
/// The panel's narrowest holds its content and, where the window draws its
/// own caption buttons, those buttons in its strip.
const LEAST: f32 = crate::material::PANEL_CONTENT_MIN_WIDTH
    + crate::window_caption::RIGHT_PANEL_CAPTION_WIDTH
    + PANE_MIN_WIDTH;

fn panel(cx: &mut VisualTestContext) -> gpui::Bounds<Pixels> {
    cx.debug_bounds("right-panel").expect("the panel")
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

/// A panel opens beside its conversation. The composer keeps the keyboard,
/// and Escape, which means something else in a docked panel, does not close
/// it.
#[gpui::test]
fn a_panel_docks_beside_the_conversation_and_leaves_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, _host, cx) = mount_panes(cx);
    let store = store_of(&shell, cx);
    one_pane(1345., cx);
    let input = typing(&shell, cx);

    open(&store, "thread-1", RightTab::Plan, cx);

    let panel_at = panel(cx);
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

/// In a pane too narrow for both, the panel covers that pane, as it is what
/// the reader sees there: it takes the keyboard, Escape closes it, and the
/// keyboard goes back.
#[gpui::test]
fn a_narrow_pane_is_covered_and_its_panel_takes_the_keyboard(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_panes(cx);
    cx.update(|_, cx| cx.set_reduce_motion(true));
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
    cx.update(|window, _| assert!(!input.is_focused(window), "the panel has the keyboard"));

    cx.simulate_keystrokes("escape");
    crate::test_frames::draw_settled(cx);
    assert_eq!(panel_of(&store, "thread-1", cx), None);
    assert_eq!(cx.debug_bounds("right-panel"), None);
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
    cx.update(|_, cx| cx.set_reduce_motion(true));
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
