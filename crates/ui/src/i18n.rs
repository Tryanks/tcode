//! Translation and locale selection for tcode.

use std::{borrow::Cow, cell::RefCell};

rust_i18n::i18n!("../../locales", fallback = "en");

// `rust_i18n::i18n!` loads the locale directory during macro expansion, but
// changes inside that directory are not reliably tracked by Cargo for normal
// (non-test) incremental builds. These anonymous includes make both locale
// files explicit compiler inputs, so adding a key always rebuilds the embedded
// translation table used by the application.
const _: &str = include_str!("../../../locales/en.yml");
const _: &str = include_str!("../../../locales/zh-CN.yml");
const _: &str = include_str!("../../../locales/ja.yml");

pub const LANGUAGE_ENGLISH: &str = "en";
pub const LANGUAGE_SIMPLIFIED_CHINESE: &str = "zh-CN";
pub const LANGUAGE_JAPANESE: &str = "ja";

thread_local! {
    /// Mobile supplies the user's first configured language through its native
    /// OS API. Desktop leaves this empty and keeps using `sys_locale`.
    static PLATFORM_SYSTEM_LOCALE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Resolve a persisted override, falling back to the supplied system locale.
pub fn resolve_locale(override_locale: Option<&str>, system_locale: Option<&str>) -> &'static str {
    match override_locale {
        Some(LANGUAGE_ENGLISH) => LANGUAGE_ENGLISH,
        Some(LANGUAGE_SIMPLIFIED_CHINESE) => LANGUAGE_SIMPLIFIED_CHINESE,
        Some(LANGUAGE_JAPANESE) => LANGUAGE_JAPANESE,
        _ => match system_locale.map(str::to_ascii_lowercase) {
            Some(locale) if locale.starts_with("zh") => LANGUAGE_SIMPLIFIED_CHINESE,
            Some(locale) if locale.starts_with("ja") => LANGUAGE_JAPANESE,
            _ => LANGUAGE_ENGLISH,
        },
    }
}

/// Resolve and apply the requested locale, returning the selected locale.
pub fn apply_locale(override_locale: Option<&str>) -> &'static str {
    let system_locale = system_locale();
    let locale = resolve_locale(override_locale, system_locale.as_deref());
    set_locale(locale);
    locale
}

pub(crate) fn set_platform_system_locale(locale: Option<&str>) {
    PLATFORM_SYSTEM_LOCALE.with(|system_locale| {
        *system_locale.borrow_mut() = locale.map(str::to_owned);
    });
}

fn system_locale() -> Option<String> {
    PLATFORM_SYSTEM_LOCALE
        .with(|locale| locale.borrow().clone())
        .or_else(sys_locale::get_locale)
}

/// The system families that draw Han characters in the forms the current
/// language reads, tried before the platform's own fallback.
///
/// A platform's own fallback picks Han glyph forms by the *system* language
/// (CoreText's cascade, DirectWrite's user locale, cosmic-text's locale), and
/// would draw a Japanese UI's kanji in Chinese forms on a Chinese or English
/// system; the UI language decides instead. English names nothing and leaves
/// CJK text to the system language, the best guess at what the user writes,
/// except on Android, whose fallback has no script preferences and would
/// otherwise reach for arbitrary serif faces, and in a browser, whose fallback
/// would take whichever of its two Han faces loaded first. gpui-base has no
/// locale-aware font selection; this is it until it does.
pub(crate) fn cjk_fallback_families() -> &'static [&'static str] {
    let locale = rust_i18n::locale();
    if &*locale == LANGUAGE_JAPANESE {
        cjk_families::JAPANESE
    } else if &*locale == LANGUAGE_SIMPLIFIED_CHINESE
        || cfg!(any(target_os = "android", target_family = "wasm"))
    {
        cjk_families::CHINESE
    } else {
        &[]
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod cjk_families {
    pub(super) const JAPANESE: &[&str] = &["Hiragino Sans"];
    pub(super) const CHINESE: &[&str] = &["PingFang SC"];
}

#[cfg(target_os = "windows")]
mod cjk_families {
    pub(super) const JAPANESE: &[&str] = &["Yu Gothic UI", "Meiryo UI"];
    pub(super) const CHINESE: &[&str] = &["Microsoft YaHei UI"];
}

/// Text takes the first of these that is loaded: Noto Sans JP once
/// [`fetch_locale_font`] has it and bundled Noto Sans SC otherwise. The system
/// fonts here serve the browser, which draws what those two lack and reads
/// this as a CSS font-family list.
#[cfg(target_family = "wasm")]
mod cjk_families {
    pub(super) const JAPANESE: &[&str] = &[
        "Hiragino Sans",
        "Yu Gothic UI",
        "Meiryo UI",
        "Noto Sans CJK JP",
        "Noto Sans JP",
    ];
    pub(super) const CHINESE: &[&str] = &["Noto Sans SC"];
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_family = "wasm"
)))]
mod cjk_families {
    pub(super) const JAPANESE: &[&str] =
        &["Noto Sans CJK JP", "Noto Sans JP", "Source Han Sans JP"];
    pub(super) const CHINESE: &[&str] = &["Noto Sans CJK SC", "Noto Sans SC", "Source Han Sans SC"];
}

/// [`cjk_fallback_families`] as the fallbacks of a font.
pub(crate) fn cjk_font_fallbacks() -> Option<gpui::FontFallbacks> {
    let families = cjk_fallback_families();
    (!families.is_empty()).then(|| {
        gpui::FontFallbacks::from_fonts(families.iter().map(|&family| family.into()).collect())
    })
}

/// The fonts [`crate::ShellOptions::locale_fonts`] still has to fetch.
struct LocaleFonts(Vec<(&'static str, String)>);

impl gpui::Global for LocaleFonts {}

pub(crate) fn set_locale_fonts(fonts: Vec<(&'static str, String)>, cx: &mut gpui::App) {
    if !fonts.is_empty() {
        cx.set_global(LocaleFonts(fonts));
    }
}

/// Fetches the current language's font, if it has one still to fetch. A font
/// that fails is not retried in this session: its language keeps the forms it
/// fell back to.
pub(crate) fn fetch_locale_font(cx: &mut gpui::App) {
    let locale = rust_i18n::locale();
    let Some(fonts) = cx.try_global::<LocaleFonts>() else {
        return;
    };
    let Some(index) = fonts.0.iter().position(|(font, _)| *font == &*locale) else {
        return;
    };
    let (_, url) = cx.global_mut::<LocaleFonts>().0.swap_remove(index);
    let client = cx.http_client();
    cx.spawn(async move |cx| {
        let font = read_font(client, &url).await;
        cx.update(|cx| match font {
            Ok(font) => {
                if let Err(error) = cx.text_system().add_fonts(vec![font.into()]) {
                    log::warn!("could not install the font at {url}: {error:#}");
                }
                cx.refresh_windows();
            }
            Err(error) => log::warn!("could not fetch the font at {url}: {error}"),
        })
    })
    .detach();
}

async fn read_font(
    client: std::sync::Arc<dyn gpui::http_client::HttpClient>,
    url: &str,
) -> Result<Vec<u8>, String> {
    use futures_lite::AsyncReadExt as _;
    let mut response = client
        .get(url, gpui::http_client::AsyncBody::empty(), true)
        .await
        .map_err(|error| format!("{error:#}"))?;
    if !response.status().is_success() {
        return Err(response.status().to_string());
    }
    let mut font = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut font)
        .await
        .map_err(|error| error.to_string())?;
    Ok(font)
}

/// The items of a list read inside a sentence, joined as the current language
/// writes one ("a, b" in English, "a、b" in Chinese and Japanese).
pub(crate) fn join_list<S: AsRef<str>>(items: impl IntoIterator<Item = S>) -> String {
    let separator = crate::tr!("list.separator");
    let mut list = String::new();
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            list.push_str(&separator);
        }
        list.push_str(item.as_ref());
    }
    list
}

/// Set the process-global translation locale.
pub fn set_locale(locale: &str) {
    rust_i18n::set_locale(locale);
}

/// Translate a key in the current locale, returning the key when it is missing.
#[doc(hidden)]
pub fn translate(key: impl AsRef<str>) -> Cow<'static, str> {
    let key = key.as_ref();
    let locale = rust_i18n::locale();
    _rust_i18n_try_translate(locale.as_ref(), key).unwrap_or_else(|| Cow::Owned(key.to_owned()))
}

/// Translate a key and replace named `%{name}` patterns with formatted values.
#[doc(hidden)]
pub fn translate_with_args(
    key: impl AsRef<str>,
    names: &[&str],
    values: &[String],
) -> Cow<'static, str> {
    let translated = translate(key);
    Cow::Owned(rust_i18n::replace_patterns(&translated, names, values))
}

/// Translate English text that reaches the UI as data (a provider option's
/// description, a reason). `key`, or a numbered variant `key.1`, `key.2`, …
/// when several sources share one key, is used only while en.yml holds exactly
/// `english` there; otherwise `english` is shown as it is. Providers share
/// descriptor ids and values, so the English text is what tells them apart.
pub(crate) fn translate_english(key: &str, english: &str) -> Cow<'static, str> {
    let locale = rust_i18n::locale();
    std::iter::once(key.to_owned())
        .chain((1..).map(|variant| format!("{key}.{variant}")))
        .enumerate()
        .map_while(|(index, key)| {
            let source = _rust_i18n_try_translate(LANGUAGE_ENGLISH, &key);
            (index == 0 || source.is_some()).then_some((key, source))
        })
        .find(|(_, source)| source.as_deref() == Some(english))
        .and_then(|(key, _)| _rust_i18n_try_translate(locale.as_ref(), &key))
        .unwrap_or_else(|| Cow::Owned(english.to_owned()))
}

/// A provider option's name in the current locale (the descriptor label, never
/// its id or values), looked up by the English name under `harness_options`.
pub(crate) fn translate_option_name(english: &str) -> String {
    let slug: String = english
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    translate_english(&format!("harness_options.{slug}"), english).into_owned()
}

pub(crate) fn translate_permission_description(key: &str, english: &str) -> String {
    let translated = translate_english(key, english).into_owned();
    if translated != english {
        return translated;
    }
    let (profile, settings) = if let Some(rest) = english.strip_prefix("Profile ")
        && let Some((profile, settings)) = rest.rsplit_once(": ")
    {
        (Some(profile), settings)
    } else {
        (None, english)
    };
    let Some(settings) = settings.strip_suffix('.') else {
        return translated;
    };
    let fragments: Vec<_> = settings.split(", ").collect();
    if fragments.len() != 3 {
        return translated;
    }
    if fragments
        .iter()
        .zip(["sandbox", "approval", "reviewer"])
        .any(|(fragment, category)| {
            !(1..)
                .map_while(|variant| {
                    _rust_i18n_try_translate(
                        LANGUAGE_ENGLISH,
                        format!("permission.current.{category}.{variant}"),
                    )
                })
                .any(|source| source == *fragment)
        })
    {
        return translated;
    }
    let sandbox = translate_english("permission.current.sandbox", fragments[0]);
    let approval = translate_english("permission.current.approval", fragments[1]);
    let reviewer = translate_english("permission.current.reviewer", fragments[2]);
    let sentence = match profile {
        Some(profile) => crate::tr!(
            "permission.current.profile",
            profile = profile,
            sandbox = sandbox,
            approval = approval,
            reviewer = reviewer
        ),
        None => crate::tr!(
            "permission.current.summary",
            sandbox = sandbox,
            approval = approval,
            reviewer = reviewer
        ),
    };
    sentence.into_owned()
}

/// Translate a key using tcode-ui's embedded locale backend.
#[macro_export]
macro_rules! tr {
    ($key:expr $(,)?) => {{
        $crate::translate($key)
    }};
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {{
        $crate::translate_with_args(
            $key,
            &[$(stringify!($name)),+],
            &[$(format!("{}", $value)),+],
        )
    }};
}

#[cfg(test)]
mod tests {
    use gpui::AppContext as _;

    use std::collections::BTreeSet;

    use super::*;

    fn keys(yaml: &str) -> BTreeSet<String> {
        let mut stack: Vec<(usize, String)> = Vec::new();
        let mut keys = BTreeSet::new();
        for line in yaml
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            let indent = line.len() - line.trim_start().len();
            let Some((name, value)) = line.trim().split_once(':') else {
                continue;
            };
            while stack.last().is_some_and(|(level, _)| *level >= indent) {
                stack.pop();
            }
            let mut path = stack
                .iter()
                .map(|(_, key)| key.as_str())
                .collect::<Vec<_>>();
            path.push(name.trim());
            if value.trim().is_empty() {
                stack.push((indent, name.trim().to_owned()));
            } else {
                keys.insert(path.join("."));
            }
        }
        keys
    }

    /// A language's font is fetched when that language is first shown, by any
    /// frame after the switch, and never for another language or twice.
    #[gpui::test]
    fn a_language_font_is_fetched_once_when_its_language_shows(cx: &mut gpui::TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let fetched = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = gpui::http_client::FakeHttpClient::create({
            let fetched = fetched.clone();
            move |request| {
                fetched.lock().unwrap().push(request.uri().to_string());
                async move {
                    Ok(gpui::http_client::Response::builder()
                        .status(200)
                        .body(b"font".to_vec().into())?)
                }
            }
        });
        cx.update(|cx| {
            cx.set_http_client(client);
            crate::theme::init(cx);
            set_locale_fonts(
                vec![(LANGUAGE_JAPANESE, "https://host/ja.otf".to_owned())],
                cx,
            );
        });
        struct Empty;
        impl gpui::Render for Empty {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                gpui::Empty
            }
        }
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| Empty);
            gpui_base::Root::new(view, window, cx)
        });
        let frame = |locale: &str, cx: &mut gpui::VisualTestContext| {
            set_locale(locale);
            crate::test_frames::draw(cx);
            cx.run_until_parked();
            fetched.lock().unwrap().len()
        };
        assert_eq!(frame(LANGUAGE_ENGLISH, cx), 0);
        assert_eq!(frame(LANGUAGE_SIMPLIFIED_CHINESE, cx), 0);
        assert_eq!(frame(LANGUAGE_JAPANESE, cx), 1);
        assert_eq!(frame(LANGUAGE_ENGLISH, cx), 1);
        assert_eq!(frame(LANGUAGE_JAPANESE, cx), 1);
    }

    #[test]
    fn locale_keys_match() {
        let en = keys(include_str!("../../../locales/en.yml"));
        let zh = keys(include_str!("../../../locales/zh-CN.yml"));
        let ja = keys(include_str!("../../../locales/ja.yml"));
        assert_eq!(en, zh, "English and zh-CN locale keys differ");
        assert_eq!(en, ja, "English and ja locale keys differ");
    }

    #[test]
    fn published_permission_text_is_translated_by_its_english() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        set_locale(LANGUAGE_SIMPLIFIED_CHINESE);
        let mut sources = std::collections::BTreeMap::new();
        for provider in agent::ProviderKind::NATIVE {
            if let Some(notice) = agent::permission_notice(provider) {
                assert_ne!(translate_english("permission.notice", notice), notice);
            }
            let Some(agent::OptionDescriptor::Select {
                id, label, options, ..
            }) = agent::permission_control(provider)
            else {
                continue;
            };
            assert_ne!(
                translate_english(&format!("permission.controls.{id}"), &label),
                label
            );
            for option in options {
                let description = option.description.unwrap();
                let key = format!("permission.values.{id}.{}", option.value);
                let translated = translate_english(&key, &description).into_owned();
                assert_ne!(translated, description, "{provider:?} {}", option.value);
                let source = sources.entry(translated).or_insert(description.clone());
                assert_eq!(*source, description, "{provider:?} {}", option.value);
            }
        }
        for (key, english) in [
            ("permission.values.permissions.profile:team", "Team sandbox"),
            ("permission.values.permissions.ask", "Reworded upstream."),
            ("permission.unavailable", "Claude started in plan instead"),
        ] {
            assert_eq!(translate_english(key, english), english);
        }
        assert_eq!(
            translate_english(
                "permission.unavailable",
                "Not available: the session started in another mode"
            ),
            "不可用：会话以另一种模式启动"
        );
        assert_eq!(
            translate_english("permission.current.label", "Current permissions"),
            "当前权限"
        );
        for (english, expected) in [
            (
                "Profile my-team: workspace sandbox, asks on request, reviewed by you.",
                "配置档 my-team：工作区沙箱，按请求询问，由你审批。",
            ),
            (
                "external sandbox, uses granular approval rules, reviewed by auto-review.",
                "外部沙箱，使用细分审批规则，由自动审查（auto-review）审批。",
            ),
            (
                "read-only sandbox, asks for untrusted actions, reviewed by you.",
                "只读沙箱，对不受信任的操作询问，由你审批。",
            ),
            (
                "full access, never asks for approval, reviewed by auto-review.",
                "完全访问，从不请求审批，由自动审查（auto-review）审批。",
            ),
        ] {
            assert_eq!(
                translate_permission_description(
                    "permission.values.permissions.effective",
                    english
                ),
                expected
            );
        }
        set_locale(LANGUAGE_ENGLISH);
    }

    #[test]
    fn explicit_overrides_win() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (platform, requested, expected) in [
            ("zh-TW", Some(LANGUAGE_ENGLISH), LANGUAGE_ENGLISH),
            (
                "en-US",
                Some(LANGUAGE_SIMPLIFIED_CHINESE),
                LANGUAGE_SIMPLIFIED_CHINESE,
            ),
            ("zh-Hans-CN", None, LANGUAGE_SIMPLIFIED_CHINESE),
            ("ja-JP", None, LANGUAGE_JAPANESE),
            ("ja-JP", Some(LANGUAGE_ENGLISH), LANGUAGE_ENGLISH),
            ("en-US", Some(LANGUAGE_JAPANESE), LANGUAGE_JAPANESE),
            ("en-US", Some("unsupported"), LANGUAGE_ENGLISH),
        ] {
            set_platform_system_locale(Some(platform));
            assert_eq!(
                apply_locale(requested),
                expected,
                "{platform} / {requested:?}"
            );
            set_platform_system_locale(None);
        }
        set_locale(LANGUAGE_ENGLISH);
    }
}
