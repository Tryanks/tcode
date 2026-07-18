use std::{ops::Range, path::Path, sync::LazyLock};

use gpui::HighlightStyle;
use gpui_component::highlighter::HighlightTheme;
use syntect::{
    easy::ScopeRangeIterator,
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(|| {
    syntect::dumps::from_uncompressed_data(include_bytes!("../assets/syntaxes.bin"))
        .expect("failed to load syntect syntax set from crates/ui/assets/syntaxes.bin")
});

/// Return the syntax name associated with `path`, or `"text"` when its
/// extension is unknown.
pub(crate) fn language_name_for_path(path: &str) -> &'static str {
    let syntax = SYNTAX_SET
        .find_syntax_for_file(Path::new(path))
        .ok()
        .flatten();

    match syntax.map(|syntax| syntax.name.as_str()) {
        Some("Rust") => "rust",
        Some("Python") => "python",
        Some("TypeScript") => "typescript",
        Some("Plain Text") | None => "text",
        // Syntax names are owned by the process-wide syntax set, so this
        // reference has the same lifetime as the static set.
        Some(name) => name,
    }
}

/// Resolve a Markdown fence token, language name, or file extension.
pub(crate) fn syntax_for_name_or_extension(name: &str) -> Option<&'static SyntaxReference> {
    let token = name.trim();
    if token.is_empty() {
        return None;
    }
    SYNTAX_SET
        .find_syntax_by_token(token)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(token))
}

/// TextMate scope prefixes ordered from most specific to least specific.
///
/// Each scope in a token's stack is examined from innermost to outermost. If
/// the theme has no style for a matching key, matching continues so a broader
/// scope can supply a style.
const SCOPE_TO_THEME_KEY: &[(&str, &str)] = &[
    ("comment.documentation", "comment.doc"),
    ("constant.character.escape", "string.escape"),
    ("constant.character", "string"),
    ("string.regexp", "string.regex"),
    ("string.quoted", "string"),
    ("string.unquoted", "string"),
    ("punctuation.section.brackets", "punctuation.bracket"),
    ("punctuation.section.braces", "punctuation.bracket"),
    ("punctuation.section.parens", "punctuation.bracket"),
    ("punctuation.separator", "punctuation.delimiter"),
    ("punctuation.terminator", "punctuation.delimiter"),
    ("variable.other.member", "property"),
    ("variable.other.property", "property"),
    ("entity.other.attribute-name", "attribute"),
    ("keyword.control.preprocessor", "preproc"),
    ("keyword.control.import", "keyword"),
    ("keyword.control", "keyword"),
    ("keyword.operator", "operator"),
    ("storage.modifier", "keyword"),
    ("storage.type.function", "keyword"),
    ("storage.type", "type"),
    ("entity.name.function", "function"),
    ("support.function", "function"),
    ("entity.name.type", "type"),
    ("entity.name.class", "type"),
    ("entity.name.struct", "type"),
    ("entity.name.enum", "type"),
    ("support.type", "type"),
    ("entity.name.tag", "tag"),
    ("entity.name.section", "title"),
    ("markup.heading", "title"),
    ("markup.bold", "emphasis.strong"),
    ("markup.italic", "emphasis"),
    ("markup.raw", "text.literal"),
    ("entity.name.label", "label"),
    ("support.class", "constructor"),
    ("support.constant", "constructor"),
    ("meta.preprocessor", "preproc"),
    ("constant.numeric", "number"),
    ("constant.language", "boolean"),
    ("constant", "constant"),
    ("variable", "variable"),
    ("keyword", "keyword"),
    ("comment", "comment"),
    ("string", "string"),
    ("punctuation", "punctuation"),
];

fn scope_matches(scope: &str, selector: &str) -> bool {
    scope == selector
        || scope
            .strip_prefix(selector)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn style_for_stack(stack: &ScopeStack, theme: &HighlightTheme) -> HighlightStyle {
    for scope in stack.scopes.iter().rev() {
        let scope = scope.to_string();
        for &(selector, key) in SCOPE_TO_THEME_KEY {
            if scope_matches(&scope, selector)
                && let Some(style) = theme.style(key)
            {
                return style;
            }
        }
    }
    HighlightStyle::default()
}

fn push_merged(
    runs: &mut Vec<(Range<usize>, HighlightStyle)>,
    range: Range<usize>,
    style: HighlightStyle,
) {
    if range.is_empty() {
        return;
    }
    if let Some((previous_range, previous_style)) = runs.last_mut()
        && previous_range.end == range.start
        && *previous_style == style
    {
        previous_range.end = range.end;
        return;
    }
    runs.push((range, style));
}

/// Highlight a complete source string, returning original UTF-8 byte ranges.
pub(crate) fn highlight_source(
    src: &str,
    lang: &str,
    theme: &HighlightTheme,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if src.is_empty() {
        return Vec::new();
    }
    let Some(syntax) = syntax_for_name_or_extension(lang) else {
        return vec![(0..src.len(), HighlightStyle::default())];
    };

    let mut parse_state = ParseState::new(syntax);
    let mut scope_stack = ScopeStack::new();
    let mut runs = Vec::new();
    let mut line_start = 0;

    for line in LinesWithEndings::from(src) {
        let parse_checkpoint = parse_state.clone();
        let stack_checkpoint = scope_stack.clone();
        let Ok(ops) = parse_state.parse_line(line, &SYNTAX_SET) else {
            parse_state = parse_checkpoint;
            push_merged(
                &mut runs,
                line_start..line_start + line.len(),
                HighlightStyle::default(),
            );
            line_start += line.len();
            continue;
        };

        let mut line_runs = Vec::new();
        let mut offset = 0;
        let mut valid = true;
        for (range, op) in ScopeRangeIterator::new(&ops, line) {
            if scope_stack.apply(op).is_err() {
                valid = false;
                break;
            }
            if !range.is_empty() {
                let start = line_start + offset;
                let end = start + range.len();
                push_merged(
                    &mut line_runs,
                    start..end,
                    style_for_stack(&scope_stack, theme),
                );
                offset += range.len();
            }
        }

        if valid && offset == line.len() {
            for (range, style) in line_runs {
                push_merged(&mut runs, range, style);
            }
        } else {
            scope_stack = stack_checkpoint;
            parse_state = parse_checkpoint;
            push_merged(
                &mut runs,
                line_start..line_start + line.len(),
                HighlightStyle::default(),
            );
        }
        line_start += line.len();
    }

    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_language_from_path() {
        assert_eq!(language_name_for_path("x.rs"), "rust");
        assert_eq!(language_name_for_path("x.py"), "python");
        assert_eq!(language_name_for_path("x.ts"), "typescript");
        assert_eq!(language_name_for_path("x.tsx"), "TypeScriptReact");
        assert_eq!(language_name_for_path("x.toml"), "TOML");
        assert_eq!(language_name_for_path("x.kt"), "Kotlin");
        assert_eq!(language_name_for_path("x.swift"), "Swift");
        assert_eq!(language_name_for_path("x.ex"), "Elixir");
        assert_eq!(language_name_for_path("x.zig"), "Zig");
        assert_eq!(language_name_for_path("noext"), "text");
        assert!(SYNTAX_SET.find_syntax_by_name("Plain Text").is_some());
    }

    #[test]
    fn bundled_dump_covers_supported_languages() {
        for language in [
            "rust",
            "python",
            "typescript",
            "tsx",
            "kotlin",
            "swift",
            "toml",
            "elixir",
            "zig",
            "cmake",
            "go",
            "javascript",
            "svelte",
            "vue",
            "protobuf",
        ] {
            assert!(
                syntax_for_name_or_extension(language).is_some(),
                "missing syntax for {language}"
            );
        }

        assert_eq!(
            syntax_for_name_or_extension("tsx").map(|syntax| syntax.name.as_str()),
            Some("TypeScriptReact")
        );
        assert_eq!(
            syntax_for_name_or_extension("jsx").map(|syntax| syntax.name.as_str()),
            Some("JavaScript (Babel)")
        );
    }

    #[test]
    fn unknown_language_uses_a_single_default_styled_run() {
        let src = "some source";
        assert_eq!(
            highlight_source(src, "unknown-language", &HighlightTheme::default_dark()),
            vec![(0..src.len(), HighlightStyle::default())]
        );
    }

    #[test]
    fn highlights_rust_with_ordered_in_bounds_runs() {
        let src = "fn ordinary() {}\n";
        let theme = HighlightTheme::default_dark();
        let runs = highlight_source(src, "rust", &theme);
        let keyword_style = theme.style("keyword").expect("default theme has keywords");

        let fn_style = runs
            .iter()
            .find(|(range, _)| range.start == 0 && range.end >= 2)
            .map(|(_, style)| *style)
            .expect("fn is covered");
        let identifier_start = src.find("ordinary").unwrap();
        let identifier_style = runs
            .iter()
            .find(|(range, _)| range.start <= identifier_start && range.end > identifier_start)
            .map(|(_, style)| *style)
            .expect("identifier is covered");

        assert_eq!(fn_style.color, keyword_style.color);
        assert_ne!(identifier_style, keyword_style);
        assert!(
            runs.iter()
                .all(|(range, _)| range.start < range.end && range.end <= src.len())
        );
        assert!(runs.windows(2).all(|pair| pair[0].0.end <= pair[1].0.start));
    }

    #[test]
    fn highlights_typescript_with_distinct_ordered_in_bounds_runs() {
        let src = "const x: number = 1;";
        let theme = HighlightTheme::default_dark();
        let runs = highlight_source(src, "typescript", &theme);

        assert!(
            runs.first().is_some_and(|(_, first_style)| runs
                .iter()
                .skip(1)
                .any(|(_, style)| style != first_style)),
            "expected at least two distinct styles"
        );
        assert!(
            runs.iter()
                .all(|(range, _)| range.start < range.end && range.end <= src.len())
        );
        assert!(runs.windows(2).all(|pair| pair[0].0.end <= pair[1].0.start));
    }
}
