//! Translation and locale selection for tcode.

use std::borrow::Cow;

rust_i18n::i18n!("../../locales", fallback = "en");

// `rust_i18n::i18n!` loads the locale directory during macro expansion, but
// changes inside that directory are not reliably tracked by Cargo for normal
// (non-test) incremental builds. These anonymous includes make both locale
// files explicit compiler inputs, so adding a key always rebuilds the embedded
// translation table used by the application.
const _: &str = include_str!("../../../locales/en.yml");
const _: &str = include_str!("../../../locales/zh-CN.yml");

pub const LANGUAGE_ENGLISH: &str = "en";
pub const LANGUAGE_SIMPLIFIED_CHINESE: &str = "zh-CN";

/// Resolve a persisted override, falling back to the supplied system locale.
pub fn resolve_locale(override_locale: Option<&str>, system_locale: Option<&str>) -> &'static str {
    match override_locale {
        Some(LANGUAGE_ENGLISH) => LANGUAGE_ENGLISH,
        Some(LANGUAGE_SIMPLIFIED_CHINESE) => LANGUAGE_SIMPLIFIED_CHINESE,
        _ if system_locale.is_some_and(|locale| locale.to_ascii_lowercase().starts_with("zh")) => {
            LANGUAGE_SIMPLIFIED_CHINESE
        }
        _ => LANGUAGE_ENGLISH,
    }
}

/// Resolve and apply the requested locale, returning the selected locale.
pub fn apply_locale(override_locale: Option<&str>) -> &'static str {
    let system_locale = sys_locale::get_locale();
    let locale = resolve_locale(override_locale, system_locale.as_deref());
    set_locale(locale);
    locale
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

/// Translate a key using tcode-i18n's embedded locale backend.
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

    #[test]
    fn locale_keys_match() {
        let en = keys(include_str!("../../../locales/en.yml"));
        let zh = keys(include_str!("../../../locales/zh-CN.yml"));
        assert_eq!(en, zh, "English and zh-CN locale keys differ");
    }

    #[test]
    fn explicit_overrides_win() {
        assert_eq!(
            resolve_locale(Some(LANGUAGE_ENGLISH), Some("zh-TW")),
            LANGUAGE_ENGLISH
        );
        assert_eq!(
            resolve_locale(Some(LANGUAGE_SIMPLIFIED_CHINESE), Some("en-US")),
            LANGUAGE_SIMPLIFIED_CHINESE
        );
        assert_eq!(
            resolve_locale(None, Some("zh-Hans-CN")),
            LANGUAGE_SIMPLIFIED_CHINESE
        );
        assert_eq!(
            resolve_locale(Some("unsupported"), Some("en-US")),
            LANGUAGE_ENGLISH
        );
    }
}
