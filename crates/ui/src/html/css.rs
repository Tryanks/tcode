//! Lightweight static CSS parser, selector matcher, media query evaluator,
//! and custom-property (`var(--...)`) resolver.

use std::collections::HashMap;

use super::dom::{Document, ElementData, NodeId, NodeKind};

#[derive(Debug, Default)]
pub(crate) struct Stylesheet {
    pub(crate) rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub(crate) struct Rule {
    pub(crate) media: Option<MediaQuery>,
    pub(crate) selectors: Vec<Selector>,
    pub(crate) declarations: Vec<Declaration>,
    pub(crate) order: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct Declaration {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) important: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct MediaQuery {
    pub(crate) branches: Vec<MediaBranch>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MediaBranch {
    pub(crate) color_scheme: Option<bool>, // Some(true) = dark, Some(false) = light
    pub(crate) min_width: Option<f32>,
    pub(crate) max_width: Option<f32>,
    pub(crate) unsupported: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MediaEnv {
    pub(crate) dark: bool,
    pub(crate) viewport_width: f32,
}

impl MediaQuery {
    pub(crate) fn matches(&self, env: MediaEnv) -> bool {
        self.branches.iter().any(|b| {
            if b.unsupported {
                return false;
            }
            if let Some(dark) = b.color_scheme
                && dark != env.dark
            {
                return false;
            }
            if let Some(min_w) = b.min_width
                && env.viewport_width < min_w
            {
                return false;
            }
            if let Some(max_w) = b.max_width
                && env.viewport_width > max_w
            {
                return false;
            }
            true
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PseudoElement {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Specificity(pub(crate) u16, pub(crate) u16, pub(crate) u16);

#[derive(Debug, Clone)]
pub(crate) struct Selector {
    /// Parts from left to right; combinator connects `parts[i-1]` to `parts[i]`.
    pub(crate) parts: Vec<(Option<Combinator>, CompoundSelector)>,
    pub(crate) pseudo_element: Option<PseudoElement>,
    pub(crate) specificity: Specificity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Combinator {
    Descendant,
    Child,
    AdjacentSibling,
    GeneralSibling,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CompoundSelector {
    pub(crate) tag: Option<String>,
    pub(crate) universal: bool,
    pub(crate) id: Option<String>,
    pub(crate) classes: Vec<String>,
    pub(crate) attrs: Vec<AttrMatcher>,
    pub(crate) pseudos: Vec<PseudoClass>,
}

#[derive(Debug, Clone)]
pub(crate) struct AttrMatcher {
    pub(crate) name: String,
    pub(crate) op: AttrOp,
    pub(crate) value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrOp {
    Exists,
    Equals,
    WordContains,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Debug, Clone)]
pub(crate) enum PseudoClass {
    Root,
    FirstChild,
    LastChild,
    OnlyChild,
    FirstOfType,
    LastOfType,
    Empty,
    NthChild(i32, i32), // a*n + b
    Not(Vec<Selector>),
    Is(Vec<Selector>),
    Where(Vec<Selector>),
    Unsupported,
}

impl Stylesheet {
    pub(crate) fn parse_all(sources: &[String]) -> Self {
        let mut sheet = Self::default();
        let mut order = 0usize;
        for src in sources {
            parse_rules(src, None, &mut sheet.rules, &mut order);
        }
        sheet
    }

    /// Collect custom-property (`--*`) declarations from `:root` / `html` / `body`.
    pub(crate) fn root_variables(&self, env: MediaEnv) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        for rule in &self.rules {
            if let Some(media) = &rule.media
                && !media.matches(env)
            {
                continue;
            }
            let targets_root = rule.selectors.iter().any(|sel| {
                sel.pseudo_element.is_none()
                    && sel.parts.len() == 1
                    && is_root_compound(&sel.parts[0].1)
            });
            if !targets_root {
                continue;
            }
            for decl in &rule.declarations {
                if decl.name.starts_with("--") {
                    vars.insert(decl.name.clone(), decl.value.clone());
                }
            }
        }
        vars
    }

    /// Collect matching declarations for `node_id` (optionally for a pseudo-element),
    /// ordered from lowest to highest cascade priority.
    pub(crate) fn matching_declarations<'a>(
        &'a self,
        doc: &Document,
        node_id: NodeId,
        env: MediaEnv,
        pseudo: Option<PseudoElement>,
    ) -> Vec<&'a Declaration> {
        let mut hits: Vec<(bool, Specificity, usize, &'a Declaration)> = Vec::new();
        for rule in &self.rules {
            if let Some(media) = &rule.media
                && !media.matches(env)
            {
                continue;
            }
            let best_spec = rule
                .selectors
                .iter()
                .filter(|sel| sel.pseudo_element == pseudo && selector_matches(sel, doc, node_id))
                .map(|sel| sel.specificity)
                .max();
            if let Some(spec) = best_spec {
                for decl in &rule.declarations {
                    hits.push((decl.important, spec, rule.order, decl));
                }
            }
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        hits.into_iter().map(|(_, _, _, d)| d).collect()
    }
}

fn is_root_compound(c: &CompoundSelector) -> bool {
    c.pseudos.iter().any(|p| matches!(p, PseudoClass::Root))
        || matches!(c.tag.as_deref(), Some("html" | "body"))
            && c.id.is_none()
            && c.classes.is_empty()
}

pub(crate) fn parse_declarations(raw: &str) -> Vec<Declaration> {
    let cleaned = strip_comments(raw);
    let mut out = Vec::new();
    for item in split_declarations(&cleaned) {
        let Some((name, val)) = item.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let name = if name.starts_with("--") {
            name.to_string()
        } else {
            name.to_ascii_lowercase()
        };
        let mut val = val.trim();
        let important = if let Some(stripped) = strip_important(val) {
            val = stripped;
            true
        } else {
            false
        };
        if !val.is_empty() {
            out.push(Declaration {
                name,
                value: val.to_string(),
                important,
            });
        }
    }
    out
}

/// Resolve all `var(--name, fallback)` references in `value` using `vars`.
pub(crate) fn resolve_vars(value: &str, vars: &HashMap<String, String>) -> String {
    resolve_vars_depth(value, vars, 0)
}

fn resolve_vars_depth(value: &str, vars: &HashMap<String, String>, depth: usize) -> String {
    if depth >= 16 || !value.contains("var(") {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(pos) = find_var_call(rest) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 4..];
        let Some(close) = find_matching_paren(after) else {
            out.push_str(&rest[pos..]);
            return out;
        };
        let inner = after[..close].trim();
        let (var_name, fallback) = split_first_top_level_comma(inner);
        let var_name = var_name.trim();
        if let Some(found) = vars.get(var_name) {
            out.push_str(&resolve_vars_depth(found.trim(), vars, depth + 1));
        } else if let Some(fb) = fallback {
            out.push_str(&resolve_vars_depth(fb.trim(), vars, depth + 1));
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn find_var_call(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    bytes.windows(4).enumerate().find_map(|(i, w)| {
        if w.eq_ignore_ascii_case(b"var(")
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'-'
                    || bytes[i - 1] == b'_'))
        {
            Some(i)
        } else {
            None
        }
    })
}

fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 1u32;
    let mut in_quote = None;
    for (i, ch) in s.char_indices() {
        if let Some(q) = in_quote {
            if ch == q {
                in_quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => in_quote = Some(ch),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_first_top_level_comma(s: &str) -> (&str, Option<&str>) {
    let mut depth = 0u32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return (&s[..i], Some(&s[i + 1..])),
            _ => {}
        }
    }
    (s, None)
}

fn strip_important(val: &str) -> Option<&str> {
    let trimmed = val.trim_end();
    if trimmed.len() >= 10 && trimmed[trimmed.len() - 10..].eq_ignore_ascii_case("!important") {
        return Some(trimmed[..trimmed.len() - 10].trim_end());
    }
    // Handle `! important` with whitespace
    if let Some(bang) = trimmed.rfind('!')
        && trimmed[bang + 1..].trim().eq_ignore_ascii_case("important")
    {
        return Some(trimmed[..bang].trim_end());
    }
    None
}

fn strip_comments(src: &str) -> String {
    if !src.contains("/*") {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        if let Some(end) = rest.find("*/") {
            rest = &rest[end + 2..];
        } else {
            return out;
        }
    }
    out.push_str(rest);
    out
}

fn parse_rules(src: &str, media: Option<MediaQuery>, out: &mut Vec<Rule>, order: &mut usize) {
    let cleaned = strip_comments(src);
    let bytes = cleaned.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        if bytes[pos] == b'@' {
            let at_start = pos;
            let (header_end, kind) = scan_to_block_or_semi(&cleaned, pos);
            let header = cleaned[at_start..header_end].trim();
            if kind == b';' {
                pos = (header_end + 1).min(bytes.len());
                continue;
            }
            if kind == b'{' {
                let block_start = header_end + 1;
                let block_end = find_matching_brace(&cleaned, block_start);
                let block = &cleaned[block_start..block_end];
                pos = (block_end + 1).min(bytes.len());
                let lower_header = header.to_ascii_lowercase();
                if let Some(query_str) = lower_header.strip_prefix("@media") {
                    let parsed_media = parse_media_query(query_str.trim());
                    let combined = combine_media(media.clone(), parsed_media);
                    parse_rules(block, Some(combined), out, order);
                } else if lower_header.starts_with("@supports")
                    || lower_header.starts_with("@layer")
                {
                    parse_rules(block, media.clone(), out, order);
                }
                // @keyframes, @font-face, etc. are safely skipped.
                continue;
            }
            break;
        }

        let sel_start = pos;
        let (brace, kind) = scan_to_block_or_semi(&cleaned, pos);
        if kind != b'{' {
            pos = (brace + 1).min(bytes.len());
            continue;
        }
        let sel_str = cleaned[sel_start..brace].trim();
        let body_start = brace + 1;
        let body_end = find_matching_brace(&cleaned, body_start);
        let body = &cleaned[body_start..body_end];
        pos = (body_end + 1).min(bytes.len());

        let selectors = parse_selector_list(sel_str);
        if selectors.is_empty() {
            continue;
        }
        let declarations = parse_declarations(body);
        if declarations.is_empty() {
            continue;
        }
        *order += 1;
        out.push(Rule {
            media: media.clone(),
            selectors,
            declarations,
            order: *order,
        });
    }
}

fn scan_to_block_or_semi(s: &str, start: usize) -> (usize, u8) {
    let bytes = s.as_bytes();
    let mut i = start;
    let mut in_quote = 0u8;
    let mut paren = 0u32;
    while i < bytes.len() {
        let b = bytes[i];
        if in_quote != 0 {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == in_quote {
                in_quote = 0;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => in_quote = b,
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'{' | b';' if paren == 0 => return (i, b),
            _ => {}
        }
        i += 1;
    }
    (bytes.len(), 0)
}

fn find_matching_brace(s: &str, start: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = start;
    let mut depth = 1u32;
    let mut in_quote = 0u8;
    while i < bytes.len() {
        let b = bytes[i];
        if in_quote != 0 {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == in_quote {
                in_quote = 0;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' | b'\'' => in_quote = b,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

fn split_declarations(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut paren = 0u32;
    let mut in_quote = None;
    for (i, ch) in s.char_indices() {
        if let Some(q) = in_quote {
            if ch == q {
                in_quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => in_quote = Some(ch),
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            ';' if paren == 0 => {
                let piece = s[start..i].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn parse_media_query(raw: &str) -> MediaQuery {
    let branches = raw
        .split(',')
        .map(|branch| {
            let mut mb = MediaBranch::default();
            let lower = branch.to_ascii_lowercase();
            if lower.contains("print") && !lower.contains("screen") {
                mb.unsupported = true;
                return mb;
            }
            let mut rest = lower.as_str();
            while let Some(open) = rest.find('(') {
                rest = &rest[open + 1..];
                let Some(close) = rest.find(')') else {
                    break;
                };
                let expr = rest[..close].trim();
                rest = &rest[close + 1..];
                if let Some((k, v)) = expr.split_once(':') {
                    let k = k.trim();
                    let v = v.trim();
                    match k {
                        "prefers-color-scheme" => {
                            if v == "dark" {
                                mb.color_scheme = Some(true);
                            } else if v == "light" {
                                mb.color_scheme = Some(false);
                            }
                        }
                        "max-width" => {
                            mb.max_width = parse_media_px(v);
                        }
                        "min-width" => {
                            mb.min_width = parse_media_px(v);
                        }
                        _ => {}
                    }
                }
            }
            mb
        })
        .collect();
    MediaQuery { branches }
}

fn parse_media_px(v: &str) -> Option<f32> {
    if let Some(n) = v.strip_suffix("px") {
        n.trim().parse::<f32>().ok()
    } else if let Some(n) = v.strip_suffix("rem").or_else(|| v.strip_suffix("em")) {
        n.trim().parse::<f32>().ok().map(|x| x * 16.0)
    } else {
        v.parse::<f32>().ok()
    }
}

fn combine_media(parent: Option<MediaQuery>, child: MediaQuery) -> MediaQuery {
    let Some(parent) = parent else {
        return child;
    };
    let mut branches = Vec::new();
    for a in &parent.branches {
        for b in &child.branches {
            let scheme = match (a.color_scheme, b.color_scheme) {
                (Some(x), Some(y)) if x != y => {
                    branches.push(MediaBranch {
                        unsupported: true,
                        ..Default::default()
                    });
                    continue;
                }
                (Some(x), _) | (_, Some(x)) => Some(x),
                (None, None) => None,
            };
            branches.push(MediaBranch {
                color_scheme: scheme,
                min_width: match (a.min_width, b.min_width) {
                    (Some(x), Some(y)) => Some(x.max(y)),
                    (x, y) => x.or(y),
                },
                max_width: match (a.max_width, b.max_width) {
                    (Some(x), Some(y)) => Some(x.min(y)),
                    (x, y) => x.or(y),
                },
                unsupported: a.unsupported || b.unsupported,
            });
        }
    }
    MediaQuery { branches }
}

fn parse_selector_list(raw: &str) -> Vec<Selector> {
    split_top_level(raw, ',')
        .into_iter()
        .filter_map(parse_selector)
        .collect()
}

fn split_top_level(input: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut paren = 0u32;
    let mut bracket = 0u32;
    let mut start = 0;
    for (i, ch) in input.char_indices() {
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            c if c == sep && paren == 0 && bracket == 0 => {
                let s = input[start..i].trim();
                if !s.is_empty() {
                    out.push(s);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn parse_selector(raw: &str) -> Option<Selector> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut pos = 0usize;
    let mut parts: Vec<(Option<Combinator>, CompoundSelector)> = Vec::new();
    let mut pending_comb: Option<Combinator> = None;
    let mut pseudo_element: Option<PseudoElement> = None;

    while pos < bytes.len() {
        let saw_ws = skip_ws_flag(bytes, &mut pos);
        if pos >= bytes.len() {
            break;
        }
        let comb = match bytes[pos] {
            b'>' => {
                pos += 1;
                Some(Combinator::Child)
            }
            b'+' => {
                pos += 1;
                Some(Combinator::AdjacentSibling)
            }
            b'~' => {
                pos += 1;
                Some(Combinator::GeneralSibling)
            }
            _ => None,
        };
        if let Some(c) = comb {
            pending_comb = Some(c);
            skip_ws_flag(bytes, &mut pos);
            continue;
        }
        if saw_ws && !parts.is_empty() && pending_comb.is_none() {
            pending_comb = Some(Combinator::Descendant);
        }
        let (compound, pe) = parse_compound(s, &mut pos)?;
        if pe.is_some() {
            pseudo_element = pe;
        }
        parts.push((pending_comb.take(), compound));
    }

    if parts.is_empty() {
        return None;
    }
    let specificity = compute_specificity(&parts, pseudo_element.is_some());
    Some(Selector {
        parts,
        pseudo_element,
        specificity,
    })
}

fn skip_ws_flag(bytes: &[u8], pos: &mut usize) -> bool {
    let start = *pos;
    while *pos < bytes.len() && bytes[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
    *pos > start
}

fn parse_compound(s: &str, pos: &mut usize) -> Option<(CompoundSelector, Option<PseudoElement>)> {
    let bytes = s.as_bytes();
    let mut compound = CompoundSelector::default();
    let mut pseudo_el = None;
    let start = *pos;

    while *pos < bytes.len() {
        let b = bytes[*pos];
        if b.is_ascii_whitespace() || matches!(b, b'>' | b'+' | b'~') {
            break;
        }
        match b {
            b'*' => {
                compound.universal = true;
                *pos += 1;
            }
            b'#' => {
                *pos += 1;
                let id = read_ident(s, pos);
                if id.is_empty() {
                    return None;
                }
                compound.id = Some(id.to_string());
            }
            b'.' => {
                *pos += 1;
                let cls = read_ident(s, pos);
                if cls.is_empty() {
                    return None;
                }
                compound.classes.push(cls.to_string());
            }
            b'[' => {
                let matcher = parse_attr_matcher(s, pos)?;
                compound.attrs.push(matcher);
            }
            b':' => {
                *pos += 1;
                let double = bytes.get(*pos) == Some(&b':');
                if double {
                    *pos += 1;
                }
                let name = read_ident(s, pos).to_ascii_lowercase();
                let arg = if bytes.get(*pos) == Some(&b'(') {
                    *pos += 1;
                    let arg_start = *pos;
                    if let Some(rel) = find_matching_paren(&s[arg_start..]) {
                        let inner = &s[arg_start..arg_start + rel];
                        *pos = arg_start + rel + 1;
                        Some(inner.trim())
                    } else {
                        *pos = bytes.len();
                        None
                    }
                } else {
                    None
                };
                match name.as_str() {
                    "before" => pseudo_el = Some(PseudoElement::Before),
                    "after" => pseudo_el = Some(PseudoElement::After),
                    "root" => compound.pseudos.push(PseudoClass::Root),
                    "first-child" => compound.pseudos.push(PseudoClass::FirstChild),
                    "last-child" => compound.pseudos.push(PseudoClass::LastChild),
                    "only-child" => compound.pseudos.push(PseudoClass::OnlyChild),
                    "first-of-type" => compound.pseudos.push(PseudoClass::FirstOfType),
                    "last-of-type" => compound.pseudos.push(PseudoClass::LastOfType),
                    "empty" => compound.pseudos.push(PseudoClass::Empty),
                    "nth-child" => {
                        if let Some((a, b)) = arg.and_then(parse_nth) {
                            compound.pseudos.push(PseudoClass::NthChild(a, b));
                        } else {
                            compound.pseudos.push(PseudoClass::Unsupported);
                        }
                    }
                    "not" => {
                        let inner = arg.map(parse_selector_list).unwrap_or_default();
                        compound.pseudos.push(PseudoClass::Not(inner));
                    }
                    "is" => {
                        let inner = arg.map(parse_selector_list).unwrap_or_default();
                        compound.pseudos.push(PseudoClass::Is(inner));
                    }
                    "where" => {
                        let inner = arg.map(parse_selector_list).unwrap_or_default();
                        compound.pseudos.push(PseudoClass::Where(inner));
                    }
                    _ => {
                        if !double {
                            compound.pseudos.push(PseudoClass::Unsupported);
                        }
                    }
                }
            }
            _ => {
                let tag = read_ident(s, pos);
                if tag.is_empty() {
                    return None;
                }
                compound.tag = Some(tag.to_ascii_lowercase());
            }
        }
    }
    if *pos == start {
        return None;
    }
    Some((compound, pseudo_el))
}

fn read_ident<'a>(s: &'a str, pos: &mut usize) -> &'a str {
    let bytes = s.as_bytes();
    let start = *pos;
    while *pos < bytes.len()
        && (bytes[*pos].is_ascii_alphanumeric()
            || matches!(bytes[*pos], b'-' | b'_')
            || bytes[*pos] >= 0x80)
    {
        *pos += 1;
    }
    &s[start..*pos]
}

fn parse_attr_matcher(s: &str, pos: &mut usize) -> Option<AttrMatcher> {
    let bytes = s.as_bytes();
    *pos += 1; // '['
    let start = *pos;
    let mut in_quote = 0u8;
    while *pos < bytes.len() {
        let b = bytes[*pos];
        if in_quote != 0 {
            if b == in_quote {
                in_quote = 0;
            }
        } else if b == b'"' || b == b'\'' {
            in_quote = b;
        } else if b == b']' {
            let inner = s[start..*pos].trim();
            *pos += 1;
            return parse_attr_inner(inner);
        }
        *pos += 1;
    }
    None
}

fn parse_attr_inner(inner: &str) -> Option<AttrMatcher> {
    if inner.is_empty() {
        return None;
    }
    for (sym, op) in [
        ("~=", AttrOp::WordContains),
        ("^=", AttrOp::Prefix),
        ("$=", AttrOp::Suffix),
        ("*=", AttrOp::Substring),
        ("=", AttrOp::Equals),
    ] {
        if let Some((name, val)) = inner.split_once(sym) {
            let val = val.trim();
            let unquoted = val
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| val.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(val);
            return Some(AttrMatcher {
                name: name.trim().to_ascii_lowercase(),
                op,
                value: unquoted.to_string(),
            });
        }
    }
    Some(AttrMatcher {
        name: inner.to_ascii_lowercase(),
        op: AttrOp::Exists,
        value: String::new(),
    })
}

fn parse_nth(raw: &str) -> Option<(i32, i32)> {
    let s: String = raw
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if s == "odd" {
        return Some((2, 1));
    }
    if s == "even" {
        return Some((2, 0));
    }
    if let Ok(b) = s.parse::<i32>() {
        return Some((0, b));
    }
    let (a_str, b_str) = s.split_once('n')?;
    let a = match a_str {
        "" | "+" => 1,
        "-" => -1,
        other => other.parse::<i32>().ok()?,
    };
    let b = if b_str.is_empty() {
        0
    } else {
        b_str.parse::<i32>().ok()?
    };
    Some((a, b))
}

fn compute_specificity(
    parts: &[(Option<Combinator>, CompoundSelector)],
    has_pseudo_element: bool,
) -> Specificity {
    let (mut a, mut b, mut c) = (0u16, 0u16, u16::from(has_pseudo_element));
    for (_, comp) in parts {
        if comp.id.is_some() {
            a = a.saturating_add(1);
        }
        b = b.saturating_add(comp.classes.len() as u16);
        b = b.saturating_add(comp.attrs.len() as u16);
        if comp.tag.is_some() {
            c = c.saturating_add(1);
        }
        for pseudo in &comp.pseudos {
            match pseudo {
                PseudoClass::Where(_) => {}
                PseudoClass::Not(list) | PseudoClass::Is(list) => {
                    if let Some(max) = list.iter().map(|s| s.specificity).max() {
                        a = a.saturating_add(max.0);
                        b = b.saturating_add(max.1);
                        c = c.saturating_add(max.2);
                    }
                }
                _ => b = b.saturating_add(1),
            }
        }
    }
    Specificity(a, b, c)
}

fn selector_matches(sel: &Selector, doc: &Document, node_id: NodeId) -> bool {
    match_parts(&sel.parts, sel.parts.len() - 1, doc, node_id)
}

fn match_parts(
    parts: &[(Option<Combinator>, CompoundSelector)],
    idx: usize,
    doc: &Document,
    node_id: NodeId,
) -> bool {
    let (comb, compound) = &parts[idx];
    if !compound_matches(compound, doc, node_id) {
        return false;
    }
    if idx == 0 {
        return true;
    }
    match comb.unwrap_or(Combinator::Descendant) {
        Combinator::Child => doc.nodes[node_id]
            .parent
            .is_some_and(|p| match_parts(parts, idx - 1, doc, p)),
        Combinator::Descendant => {
            let mut cur = doc.nodes[node_id].parent;
            while let Some(p) = cur {
                if match_parts(parts, idx - 1, doc, p) {
                    return true;
                }
                cur = doc.nodes[p].parent;
            }
            false
        }
        Combinator::AdjacentSibling => doc
            .prev_element_sibling(node_id)
            .is_some_and(|prev| match_parts(parts, idx - 1, doc, prev)),
        Combinator::GeneralSibling => {
            let mut cur = doc.prev_element_sibling(node_id);
            while let Some(prev) = cur {
                if match_parts(parts, idx - 1, doc, prev) {
                    return true;
                }
                cur = doc.prev_element_sibling(prev);
            }
            false
        }
    }
}

fn compound_matches(comp: &CompoundSelector, doc: &Document, node_id: NodeId) -> bool {
    let Some(el) = doc.element(node_id) else {
        return false;
    };
    if let Some(tag) = &comp.tag
        && el.tag != *tag
    {
        return false;
    }
    if let Some(id) = &comp.id
        && el.id.as_deref() != Some(id.as_str())
    {
        return false;
    }
    for cls in &comp.classes {
        if !el.has_class(cls) {
            return false;
        }
    }
    for attr in &comp.attrs {
        if !attr_matches(attr, el) {
            return false;
        }
    }
    for pseudo in &comp.pseudos {
        if !pseudo_matches(pseudo, doc, node_id, el) {
            return false;
        }
    }
    true
}

fn attr_matches(attr: &AttrMatcher, el: &ElementData) -> bool {
    let Some(val) = el.attr(&attr.name) else {
        return false;
    };
    match attr.op {
        AttrOp::Exists => true,
        AttrOp::Equals => val == attr.value,
        AttrOp::WordContains => val.split_whitespace().any(|w| w == attr.value),
        AttrOp::Prefix => !attr.value.is_empty() && val.starts_with(&attr.value),
        AttrOp::Suffix => !attr.value.is_empty() && val.ends_with(&attr.value),
        AttrOp::Substring => !attr.value.is_empty() && val.contains(&attr.value),
    }
}

fn pseudo_matches(pseudo: &PseudoClass, doc: &Document, node_id: NodeId, el: &ElementData) -> bool {
    match pseudo {
        PseudoClass::Root => doc.nodes[node_id].parent.is_none(),
        PseudoClass::FirstChild => doc.element_sibling_position(node_id).0 == 1,
        PseudoClass::LastChild => {
            let (pos, total) = doc.element_sibling_position(node_id);
            pos > 0 && pos == total
        }
        PseudoClass::OnlyChild => doc.element_sibling_position(node_id) == (1, 1),
        PseudoClass::FirstOfType => doc.element_type_position(node_id, &el.tag).0 == 1,
        PseudoClass::LastOfType => {
            let (pos, total) = doc.element_type_position(node_id, &el.tag);
            pos > 0 && pos == total
        }
        PseudoClass::Empty => doc.nodes[node_id]
            .children
            .iter()
            .all(|&c| matches!(&doc.nodes[c].kind, NodeKind::Text(t) if t.is_empty())),
        PseudoClass::NthChild(a, b) => {
            let pos = doc.element_sibling_position(node_id).0 as i32;
            if pos <= 0 {
                return false;
            }
            if *a == 0 {
                pos == *b
            } else {
                let diff = pos - *b;
                diff % *a == 0 && diff / *a >= 0
            }
        }
        PseudoClass::Not(list) => !list.iter().any(|s| selector_matches(s, doc, node_id)),
        PseudoClass::Is(list) | PseudoClass::Where(list) => {
            list.iter().any(|s| selector_matches(s, doc, node_id))
        }
        PseudoClass::Unsupported => false,
    }
}
