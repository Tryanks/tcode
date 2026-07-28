//! Sanitize a syntect syntax-set dump for the fancy-regex backend.
//!
//! `assets/syntaxes.bin` originates from bat, whose syntax set is pre-tested
//! against the Oniguruma backend. tcode compiles syntect with `regex-fancy`,
//! and fancy-regex rejects some Oniguruma-only constructs (subroutine calls,
//! variable-length look-behind, `\o{}` escapes, ...). syntect compiles each
//! pattern lazily on first use and panics on failure (`regex.rs:70`,
//! "regex string should be pre-tested"), which crashed the app in the field.
//!
//! This tool replaces every pattern that fails to compile under the active
//! backend with `(?!)` — valid, and it never matches — so the affected rule is
//! disabled instead of being a runtime landmine. Patterns with `\N` capture
//! back-references are skipped: syntect substitutes the captured text before
//! compiling those, so the raw string never reaches the regex compiler.
//!
//! Usage: cargo run -p tcode-ui --example sanitize_syntaxes -- <in.bin> <out.bin>

use syntect::parsing::syntax_definition::Pattern;
use syntect::parsing::{Regex, SyntaxSet, SyntaxSetBuilder};

const NEVER_MATCHES: &str = "(?!)";

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: sanitize_syntaxes <in.bin> <out.bin>");
    let output = args
        .next()
        .expect("usage: sanitize_syntaxes <in.bin> <out.bin>");

    assert!(
        Regex::try_compile(NEVER_MATCHES).is_none(),
        "replacement pattern must compile under the active regex backend"
    );
    assert!(!Regex::new(NEVER_MATCHES.into()).is_match("probe"));

    let data = std::fs::read(&input).expect("read input dump");
    let set: SyntaxSet =
        syntect::dumps::from_uncompressed_data(&data).expect("deserialize input dump");
    let syntax_count = set.syntaxes().len();

    let mut replaced = 0usize;
    let mut builder = SyntaxSetBuilder::new();
    for syntax in set.into_builder().syntaxes() {
        let mut syntax = syntax.clone();
        if let Some(first_line) = &syntax.first_line_match
            && let Some(err) = Regex::try_compile(first_line)
        {
            println!("{}: first_line_match: {err}", syntax.name);
            syntax.first_line_match = None;
            replaced += 1;
        }
        for context in syntax.contexts.values_mut() {
            for pattern in &mut context.patterns {
                let Pattern::Match(pattern) = pattern else {
                    continue;
                };
                // Back-reference patterns are compiled only after capture
                // substitution; their raw strings legitimately fail here.
                if pattern.has_captures {
                    continue;
                }
                if let Some(err) = Regex::try_compile(pattern.regex.regex_str()) {
                    println!(
                        "{}: {err}: {}",
                        syntax.name,
                        pattern.regex.regex_str().escape_debug()
                    );
                    pattern.regex = Regex::new(NEVER_MATCHES.into());
                    replaced += 1;
                }
            }
        }
        builder.add(syntax);
    }

    let sanitized = builder.build();
    assert_eq!(
        sanitized.syntaxes().len(),
        syntax_count,
        "syntaxes lost in rebuild"
    );
    // Uncompressed on purpose: highlight.rs loads this via
    // `from_uncompressed_data` (the lazy per-syntax context blobs inside are
    // already compressed, so an outer zlib layer only costs startup time).
    syntect::dumps::dump_to_uncompressed_file(&sanitized, &output).expect("write output dump");

    // Prove the shipped bytes are clean: reload the file we just wrote and
    // re-run the compile check over every pattern.
    let bytes = std::fs::read(&output).expect("re-read output dump");
    let reloaded: SyntaxSet =
        syntect::dumps::from_uncompressed_data(&bytes).expect("reload sanitized dump");
    let mut remaining = 0usize;
    for syntax in reloaded.into_builder().syntaxes() {
        for context in syntax.contexts.values() {
            for pattern in &context.patterns {
                if let Pattern::Match(pattern) = pattern
                    && !pattern.has_captures
                    && Regex::try_compile(pattern.regex.regex_str()).is_some()
                {
                    remaining += 1;
                }
            }
        }
    }
    assert_eq!(
        remaining, 0,
        "sanitized set still has uncompilable patterns"
    );

    println!("replaced {replaced} patterns across {syntax_count} syntaxes -> {output}");
}
