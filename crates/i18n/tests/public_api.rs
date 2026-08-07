#[test]
fn exported_macro_translates_and_interpolates() {
    tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
    assert_eq!(tcode_i18n::tr!("chat.new_thread"), "New thread");
    assert_eq!(
        tcode_i18n::tr!("chat.hide_previous_logs", count = 4),
        "Hide 4 previous log entries"
    );

    tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);
    assert_eq!(tcode_i18n::tr!("chat.new_thread"), "新建对话");
    assert_eq!(
        tcode_i18n::tr!("chat.hide_previous_logs", count = 4),
        "收起前面的 4 条日志"
    );
    tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
}
