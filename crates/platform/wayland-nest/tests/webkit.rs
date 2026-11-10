//! A WebKitGTK page on the nest (see `common/page.rs`) with GTK as the
//! preview starts it.
//!
//! GTK initializes once per process, so this binary holds one test.
#![cfg(target_os = "linux")]

mod common;

use common::page::*;
use wayland_nest::{Event, modifier};

#[test]
fn a_webkit_page_on_the_nest() {
    let _runtime = common::runtime_dir();
    let mut page = Page::open();

    // A frame of the configured size, once the page draws.
    let frame = page.frame.as_ref().expect("a frame");
    assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));

    // The <select>'s menu composites over the page below it, and a press
    // outside dismisses it.
    let below_select = (60, 120);
    page.click(60.0, 24.0);
    page.run_until("the select's menu", |_, frame| {
        pixel(frame, below_select.0, below_select.1) != BACKGROUND
    });
    page.click(380.0, 280.0);
    page.run_until("the menu to close", |_, frame| {
        pixel(frame, below_select.0, below_select.1) == BACKGROUND
    });

    // Keys, then text an input method composes and commits.
    page.focus_field();
    page.press(&[], "a");
    page.press(&[], "B");
    page.run_until("typed keys", |state, _| state["value"] == "aB");
    page.nest.compose_text("にほ".into(), Some(6..6));
    page.run_until("a composition", |state, _| {
        state["composed"]
            .as_array()
            .is_some_and(|composed| composed.iter().any(|text| text == "にほ"))
    });
    page.nest.commit_text("日本".into());
    page.nest.commit_text("é漢字".into());
    page.run_until("committed text", |state, _| state["value"] == "aB日本é漢字");
    // Keys and characters the US layout lacks, typed faster than GTK reads
    // them, arrive in order.
    let typed = " Ünïcödé 漢字!";
    page.type_mixed(typed);
    let expected = format!("aB日本é漢字{typed}");
    page.run_until("the mixed text", |state, _| {
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

    // The host's clipboard pastes in the page, and what the page copies
    // afterwards, each time, reaches the host.
    page.nest.set_clipboard("pasted ✓".into());
    page.press(&[modifier::CONTROL], "a");
    page.press(&[modifier::CONTROL], "v");
    page.run_until("the pasted text", |state, _| state["value"] == "pasted ✓");
    for (typed, value) in [("!", "pasted ✓!"), ("?", "pasted ✓!?")] {
        page.press(&[], "end");
        page.press(&[], typed);
        page.run_until("the typed text", |state, _| state["value"] == value);
        page.press(&[modifier::CONTROL], "a");
        page.press(&[modifier::CONTROL], "c");
        let copied = common::wait_for(&page.nest, TIMEOUT, pump_gtk, |event| match event {
            Event::Clipboard(text) => Some(text),
            Event::Frame { .. } => {
                page.nest.presented(VIEW);
                None
            }
            _ => None,
        });
        assert_eq!(copied.as_deref(), Some(value));
    }
}
