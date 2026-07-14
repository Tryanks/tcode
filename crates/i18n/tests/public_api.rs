use std::{cell::Cell, fmt};

#[test]
fn exported_macro_anchors_translation_in_i18n_crate() {
    tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
    assert_eq!(tcode_i18n::tr!("chat.new_thread"), "New thread");

    let dynamic_key = "chat.no_active_thread";
    assert_eq!(tcode_i18n::tr!(dynamic_key,), "No active thread");
    let dynamic_key_reference = &dynamic_key;
    assert_eq!(tcode_i18n::tr!(*dynamic_key_reference), "No active thread");

    let key_calls = Cell::new(0);
    let key_result = || {
        key_calls.set(key_calls.get() + 1);
        "chat.work_log"
    };
    assert_eq!(tcode_i18n::tr!(key_result()), "Work Log");
    assert_eq!(key_calls.get(), 1);

    struct CountedDisplay<'a> {
        calls: &'a Cell<usize>,
        value: &'static str,
    }

    impl fmt::Display for CountedDisplay<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.calls.set(self.calls.get() + 1);
            formatter.write_str(self.value)
        }
    }

    let display_calls = Cell::new(0);
    assert_eq!(
        tcode_i18n::tr!(
            if true {
                "sidebar.remove_project_description"
            } else {
                "missing"
            },
            project = CountedDisplay {
                calls: &display_calls,
                value: "demo"
            },
            count = 2,
        ),
        "This removes the project and its 2 threads from tcode. Files on disk are not touched."
    );
    assert_eq!(display_calls.get(), 1);
    assert_eq!(tcode_i18n::tr!("missing.key"), "missing.key");

    tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);
    assert_eq!(tcode_i18n::tr!("chat.new_thread"), "新建对话");
    tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
}
