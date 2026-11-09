//! The page's text input stays with the nest when the session names another
//! GTK input method module, as fcitx's and IBus's users set GTK_IM_MODULE.
//! "ipa" stands in for theirs: GTK 3 ships it, and like theirs it neither
//! speaks text-input-v3 nor passes the nest's keys through untouched.
//!
//! GTK initializes once per process, so this binary holds one test.
#![cfg(target_os = "linux")]

mod common;

use common::page::*;

#[test]
fn input_method_text_reaches_the_page_whatever_gtk_im_module_names() {
    let _runtime = common::runtime_dir();
    // SAFETY: see `common::runtime_dir`; no other thread is running yet.
    unsafe { std::env::set_var("GTK_IM_MODULE", "ipa") };
    let mut page = Page::open();
    page.focus_field();

    page.nest.compose_text("にほ".into(), Some(6..6));
    page.run_until("a composition", |state, _| {
        state["composed"]
            .as_array()
            .is_some_and(|composed| composed.iter().any(|text| text == "にほ"))
    });
    page.nest.commit_text("日本".into());
    page.run_until("committed text", |state, _| state["value"] == "日本");

    let typed = " Ünïcödé 漢字!";
    page.type_mixed(typed);
    let expected = format!("日本{typed}");
    page.run_until("the mixed text", |state, _| {
        state["value"]
            .as_str()
            .is_some_and(|value| value.chars().count() >= expected.chars().count())
    });
    assert_eq!(page.state()["value"], expected.as_str());
}
