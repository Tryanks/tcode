//! A WebKitGTK page on the nest, hosted in a session (see `common`) as the
//! preview hosts it.
//!
//! GTK initializes once per process, so this binary holds one test.
#![cfg(target_os = "linux")]

mod common;

use common::page::*;
use common::{HEIGHT, WIDTH};
use wayland_nest::modifier;

#[test]
fn a_webkit_page_on_the_nest() {
    let mut page = Page::open();

    // The session's pointer opens the <select>'s menu, which is drawn over
    // the page below it, and a press outside dismisses it.
    let below_select = (60, 120);
    page.session.click(60, 24);
    page.until_shown(
        "the select's menu",
        below_select.0,
        below_select.1,
        |shown| shown != BACKGROUND,
    );
    page.session.click(700, 500);
    page.until_shown(
        "the menu to close",
        below_select.0,
        below_select.1,
        |shown| shown == BACKGROUND,
    );

    // The file chooser the page opens is a window of its own, which the
    // session shows in the middle of the view; it takes the keyboard, and
    // once Escape has closed it the page takes input again.
    let middle = (WIDTH / 2, HEIGHT / 2);
    page.session.click(40, 410);
    page.until_shown("the file chooser", middle.0, middle.1, |shown| {
        shown != BACKGROUND
    });
    page.press(&[], "escape");
    page.until_shown("the file chooser to close", middle.0, middle.1, |shown| {
        shown == BACKGROUND
    });

    // Keys, then text an input method composes and commits.
    page.focus_field();
    page.press(&[], "a");
    page.press(&[], "B");
    page.run_until("typed keys", |state| state["value"] == "aB");
    page.nest.compose_text("にほ".into(), Some(6..6));
    page.run_until("a composition", |state| {
        state["composed"]
            .as_array()
            .is_some_and(|composed| composed.iter().any(|text| text == "にほ"))
    });
    page.nest.commit_text("日本".into());
    page.nest.commit_text("é漢字".into());
    page.run_until("committed text", |state| state["value"] == "aB日本é漢字");
    // Keys and characters the US layout lacks, typed faster than GTK reads
    // them, arrive in order.
    let typed = " Ünïcödé 漢字!";
    page.type_mixed(typed);
    let expected = format!("aB日本é漢字{typed}");
    page.run_until("the mixed text", |state| {
        state["value"]
            .as_str()
            .is_some_and(|value| value.chars().count() >= expected.chars().count())
    });
    assert_eq!(page.state()["value"], expected.as_str());

    // Typed keys reach the page as keys, before what WebKit reports for the
    // input method's text.
    let state = page.state();
    let keys = state["keys"]
        .as_array()
        .map(|keys| &keys[..keys.len().min(3)]);
    assert_eq!(
        keys,
        Some(&["a".into(), "Shift".into(), "B".into()][..]),
        "keys: {}",
        state["keys"]
    );

    // The page pastes what the host's clipboard holds when it pastes, and
    // what it copies afterwards, each time, reaches the host.
    page.clipboard = Some("pasted ✓".into());
    page.press(&[modifier::CONTROL], "a");
    page.press(&[modifier::CONTROL], "v");
    page.run_until("the pasted text", |state| state["value"] == "pasted ✓");
    for (typed, value) in [("!", "pasted ✓!"), ("?", "pasted ✓!?")] {
        page.press(&[], "end");
        page.press(&[], typed);
        page.run_until("the typed text", |state| state["value"] == value);
        page.press(&[modifier::CONTROL], "a");
        page.press(&[modifier::CONTROL], "c");
        assert_eq!(page.next_copy(), value);
    }
    // The host's clipboard changed behind the page's back: the page pastes
    // the new text, not what it copied.
    page.clipboard = Some("from the host".into());
    page.press(&[modifier::CONTROL], "a");
    page.press(&[modifier::CONTROL], "v");
    page.run_until("the host's text", |state| state["value"] == "from the host");
}
