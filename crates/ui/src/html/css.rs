//! Lightweight static CSS parser, selector matcher, media query evaluator,
//! and custom-property (`var(--...)`) resolver.

use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;

use super::dom::{Document, ElementData, NodeId, NodeKind};

/// How deep at-rules, `:not()` / `:is()` / `:where()` arguments, `var()`
/// chains and fallbacks, and colour functions nest before what is inside is
/// left out: each level is a level of recursion here.
pub(crate) const MAX_NESTING: usize = 16;

/// The longest value a `var()` substitution may produce. References multiply:
/// sixteen properties that each use the next one four times are four billion
/// copies of the last.
pub(crate) const MAX_VALUE_BYTES: usize = 4096;

/// A number as CSS and HTML attributes write one. Rust also reads `NaN` and
/// `inf`, which no layout and no colour survives.
pub(crate) fn number(raw: &str) -> Option<f32> {
    raw.trim().parse::<f32>().ok().filter(|n| n.is_finite())
}

/// The work one cascade may do, counted in compound selectors tested and
/// declarations applied. A stylesheet is as long as its author likes and
/// every rule in it is tried on every element, so the product has to end
/// somewhere; [`Budget::ran_out`] says when it did.
pub(crate) struct Budget {
    left: usize,
    ran_out: bool,
}

impl Budget {
    pub(crate) const CASCADE: usize = 2_000_000;

    pub(crate) fn new(units: usize) -> Self {
        Self {
            left: units,
            ran_out: false,
        }
    }

    pub(crate) fn spend(&mut self, units: usize) -> bool {
        match self.left.checked_sub(units) {
            Some(left) => self.left = left,
            None => self.ran_out = true,
        }
        !self.ran_out
    }

    pub(crate) fn ran_out(&self) -> bool {
        self.ran_out
    }
}

#[derive(Debug, Default)]
pub(crate) struct Stylesheet {
    pub(crate) rules: Vec<Rule>,
    /// The `@media` lists rules sit under: a rule inside nested ones holds
    /// every list on the way to it, and applies when each of them matches.
    conditions: Vec<Vec<MediaQuery>>,
    /// Rules nested deeper than [`MAX_NESTING`] were not read.
    pub(crate) left_out: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Rule {
    condition: Option<usize>,
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
struct MediaQuery {
    branches: Vec<MediaBranch>,
}

#[derive(Debug, Clone)]
struct MediaBranch {
    negated: bool,
    /// It names a feature this renderer does not evaluate, so it never
    /// matches, negated or not.
    unknown: bool,
    screen: bool,
    color_scheme: Option<bool>, // Some(true) = dark, Some(false) = light
    min_width: Option<f32>,
    max_width: Option<f32>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MediaEnv {
    pub(crate) dark: bool,
    pub(crate) viewport_width: f32,
}

impl MediaQuery {
    fn matches(&self, env: MediaEnv) -> bool {
        self.branches.iter().any(|b| {
            let holds = b.screen
                && b.color_scheme.is_none_or(|dark| dark == env.dark)
                && b.min_width.is_none_or(|min| env.viewport_width >= min)
                && b.max_width.is_none_or(|max| env.viewport_width <= max);
            !b.unknown && holds != b.negated
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
    /// Two of its combinators each try every ancestor or earlier sibling, so
    /// a failed match can try one node for one part many times over.
    retries: bool,
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
    /// A pseudo-class or pseudo-element this renderer has no state for: the
    /// selector matches nothing.
    Unsupported,
}

impl Stylesheet {
    pub(crate) fn parse_all(sources: &[String]) -> Self {
        let mut sheet = Self::default();
        let mut order = 0usize;
        for src in sources {
            parse_rules(&strip_comments(src), None, 0, &mut sheet, &mut order);
        }
        sheet
    }

    /// Which of the sheet's `@media` lists hold in `env`. Two environments
    /// with the same answer cascade alike.
    pub(crate) fn conditions(&self, env: MediaEnv) -> Vec<bool> {
        self.conditions
            .iter()
            .map(|lists| lists.iter().all(|query| query.matches(env)))
            .collect()
    }

    /// The rules that apply under `conditions`, the answer [`Self::conditions`]
    /// gave, with the selectors each of the three cascades matches: an
    /// element's own and its `::before` and `::after`.
    pub(crate) fn active(&self, conditions: &[bool]) -> ActiveRules<'_> {
        let mut active = ActiveRules::default();
        for rule in &self.rules {
            if rule.condition.is_some_and(|ix| !conditions[ix]) {
                continue;
            }
            for (pass, pseudo) in [
                None,
                Some(PseudoElement::Before),
                Some(PseudoElement::After),
            ]
            .into_iter()
            .enumerate()
            {
                let selectors: Vec<&Selector> = rule
                    .selectors
                    .iter()
                    .filter(|sel| sel.pseudo_element == pseudo)
                    .collect();
                if !selectors.is_empty() {
                    active.passes[pass].push((rule, selectors));
                }
            }
        }
        active
    }
}

#[derive(Default)]
pub(crate) struct ActiveRules<'a> {
    /// Every selector listed is tested when its pass visits an element, and
    /// every test is paid for from the [`Budget`].
    passes: [Vec<(&'a Rule, Vec<&'a Selector>)>; 3],
}

impl<'a> ActiveRules<'a> {
    /// The custom properties `:root` / `html` / `body` rules declare, in
    /// rule order.
    pub(crate) fn root_properties(&self) -> Vec<(&'a str, &'a str)> {
        let mut declared = Vec::new();
        for (rule, selectors) in &self.passes[0] {
            let targets_root = selectors
                .iter()
                .any(|sel| sel.parts.len() == 1 && is_root_compound(&sel.parts[0].1));
            if !targets_root {
                continue;
            }
            for decl in &rule.declarations {
                if decl.name.starts_with("--") {
                    declared.push((decl.name.as_str(), decl.value.as_str()));
                }
            }
        }
        declared
    }

    /// Collect matching declarations for `node_id` (optionally for a pseudo-element),
    /// ordered from lowest to highest cascade priority.
    pub(crate) fn matching_declarations(
        &self,
        doc: &Document,
        node_id: NodeId,
        pseudo: Option<PseudoElement>,
        budget: &mut Budget,
    ) -> Vec<&'a Declaration> {
        let pass = match pseudo {
            None => 0,
            Some(PseudoElement::Before) => 1,
            Some(PseudoElement::After) => 2,
        };
        let mut matching = Matching {
            doc,
            budget,
            seen: HashMap::new(),
        };
        let mut hits: Vec<(bool, Specificity, usize, &'a Declaration)> = Vec::new();
        for (rule, selectors) in &self.passes[pass] {
            if matching.budget.ran_out() {
                break;
            }
            let best_spec = selectors
                .iter()
                .filter(|sel| matching.selector(sel, node_id))
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

/// The custom properties in scope for an element: its own over those it
/// inherits. Each value is resolved where it is declared, so none holds a
/// `var()` and a reference costs one lookup however the declarations chain.
#[derive(Debug, Default)]
pub(crate) struct Vars {
    own: HashMap<String, Rc<str>>,
    inherited: Option<Rc<Vars>>,
}

impl Vars {
    pub(crate) fn get(&self, name: &str) -> Option<&Rc<str>> {
        let mut scope = self;
        loop {
            if let Some(value) = scope.own.get(name) {
                return Some(value);
            }
            scope = scope.inherited.as_deref()?;
        }
    }

    /// The scope of an element that declares `declared`, the later of two
    /// declarations of a name winning, under `inherited`. Declarations may
    /// use each other in any order. One that cannot be resolved (it uses a
    /// name with no value and gives no fallback, is part of a cycle, chains
    /// deeper than [`MAX_NESTING`] or grows past [`MAX_VALUE_BYTES`]) is not
    /// declared, so the inherited value, if any, stays in scope, as in CSS.
    pub(crate) fn declare(inherited: &Rc<Self>, declared: &[(&str, &str)]) -> Rc<Self> {
        let mut declaring = Declaring {
            raw: declared.iter().copied().collect(),
            inherited,
            resolved: HashMap::new(),
            resolving: Vec::new(),
        };
        for (name, _) in declared {
            declaring.resolve(name);
        }
        Rc::new(Self {
            own: declaring
                .resolved
                .into_iter()
                .filter_map(|(name, value)| Some((name.to_owned(), value?)))
                .collect(),
            inherited: Some(inherited.clone()),
        })
    }
}

trait Lookup {
    fn var(&mut self, name: &str) -> Option<Rc<str>>;
}

impl Lookup for &Vars {
    fn var(&mut self, name: &str) -> Option<Rc<str>> {
        self.get(name).cloned()
    }
}

struct Declaring<'a> {
    raw: HashMap<&'a str, &'a str>,
    inherited: &'a Vars,
    resolved: HashMap<&'a str, Option<Rc<str>>>,
    resolving: Vec<&'a str>,
}

impl Declaring<'_> {
    fn resolve(&mut self, name: &str) -> Option<Rc<str>> {
        let (&name, &raw) = self.raw.get_key_value(name)?;
        if let Some(resolved) = self.resolved.get(name) {
            return resolved.clone();
        }
        if self.resolving.contains(&name) || self.resolving.len() >= MAX_NESTING {
            return None;
        }
        self.resolving.push(name);
        let mut value = String::new();
        let resolved = substitute(raw, self, 0, true, MAX_VALUE_BYTES, &mut value)
            .map(|()| Rc::from(value.trim()));
        self.resolving.pop();
        self.resolved.insert(name, resolved.clone());
        resolved
    }
}

impl Lookup for Declaring<'_> {
    fn var(&mut self, name: &str) -> Option<Rc<str>> {
        if self.raw.contains_key(name) {
            self.resolve(name)
        } else {
            self.inherited.get(name).cloned()
        }
    }
}

/// A declaration's value with each `var()` replaced, or `None` when one of
/// them has neither a value nor a fallback, or the result is longer than
/// [`MAX_VALUE_BYTES`]: the declaration is then invalid, as in CSS.
pub(crate) fn resolve_vars<'v>(value: &'v str, vars: &Vars) -> Option<Cow<'v, str>> {
    if find_var_call(value).is_none() {
        return Some(Cow::Borrowed(value));
    }
    let mut out = String::with_capacity(value.len());
    substitute(value, &mut &*vars, 0, true, MAX_VALUE_BYTES, &mut out)?;
    Some(Cow::Owned(out))
}

/// A whole document's text (a stylesheet, an inline SVG) with each `var()`
/// replaced, one without a value by nothing. `None` when the result would be
/// longer than `limit`.
pub(crate) fn resolve_vars_in_text(text: &str, vars: &Vars, limit: usize) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    substitute(text, &mut &*vars, 0, false, limit, &mut out)?;
    Some(out)
}

fn substitute(
    value: &str,
    vars: &mut dyn Lookup,
    depth: usize,
    strict: bool,
    limit: usize,
    out: &mut String,
) -> Option<()> {
    let push = |out: &mut String, piece: &str| {
        (out.len() + piece.len() <= limit).then(|| out.push_str(piece))
    };
    let mut rest = value;
    while let Some(pos) = find_var_call(rest) {
        push(out, &rest[..pos])?;
        let after = &rest[pos + 4..];
        let Some(close) = find_matching_paren(after) else {
            return push(out, &rest[pos..]);
        };
        let (name, fallback) = split_first_top_level_comma(&after[..close]);
        match (vars.var(name.trim()), fallback) {
            (Some(found), _) => push(out, &found)?,
            (None, Some(fallback)) if depth < MAX_NESTING => {
                substitute(fallback.trim(), vars, depth + 1, strict, limit, out)?
            }
            _ if strict => return None,
            _ => {}
        }
        rest = &after[close + 1..];
    }
    push(out, rest)
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
    let (value, flag) = val.rsplit_once('!')?;
    flag.trim()
        .eq_ignore_ascii_case("important")
        .then(|| value.trim_end())
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

fn parse_rules(
    cleaned: &str,
    condition: Option<usize>,
    depth: usize,
    sheet: &mut Stylesheet,
    order: &mut usize,
) {
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
            let (header_end, kind) = scan_to_block_or_semi(cleaned, pos);
            let header = cleaned[at_start..header_end].trim();
            if kind == b';' {
                pos = (header_end + 1).min(bytes.len());
                continue;
            }
            if kind == b'{' {
                let block_start = header_end + 1;
                let block_end = find_matching_brace(cleaned, block_start);
                let block = &cleaned[block_start..block_end];
                pos = (block_end + 1).min(bytes.len());
                let lower_header = header.to_ascii_lowercase();
                let inner = if let Some(query) = lower_header.strip_prefix("@media") {
                    let mut lists = condition
                        .map(|ix| sheet.conditions[ix].clone())
                        .unwrap_or_default();
                    lists.push(parse_media_query(query));
                    sheet.conditions.push(lists);
                    Some(Some(sheet.conditions.len() - 1))
                } else if lower_header.starts_with("@supports")
                    || lower_header.starts_with("@layer")
                {
                    Some(condition)
                } else {
                    // @keyframes, @font-face, etc. are safely skipped.
                    None
                };
                match inner {
                    Some(_) if depth >= MAX_NESTING => sheet.left_out = true,
                    Some(condition) => parse_rules(block, condition, depth + 1, sheet, order),
                    None => {}
                }
                continue;
            }
            break;
        }

        let sel_start = pos;
        let (brace, kind) = scan_to_block_or_semi(cleaned, pos);
        if kind != b'{' {
            pos = (brace + 1).min(bytes.len());
            continue;
        }
        let sel_str = cleaned[sel_start..brace].trim();
        let body_start = brace + 1;
        let body_end = find_matching_brace(cleaned, body_start);
        let body = &cleaned[body_start..body_end];
        pos = (body_end + 1).min(bytes.len());

        let selectors = parse_selector_list(sel_str, 0, &mut sheet.left_out);
        if selectors.is_empty() {
            continue;
        }
        let declarations = parse_declarations(body);
        if declarations.is_empty() {
            continue;
        }
        *order += 1;
        sheet.rules.push(Rule {
            condition,
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
    MediaQuery {
        branches: raw.split(',').map(parse_media_branch).collect(),
    }
}

fn parse_media_branch(raw: &str) -> MediaBranch {
    let mut branch = MediaBranch {
        negated: false,
        unknown: false,
        screen: true,
        color_scheme: None,
        min_width: None,
        max_width: None,
    };
    let mut rest = raw.trim_start();
    while !rest.is_empty() {
        if let Some(feature) = rest.strip_prefix('(') {
            let Some(close) = find_matching_paren(feature) else {
                branch.unknown = true;
                break;
            };
            let known = match feature[..close].split_once(':') {
                Some((name, value)) => match (name.trim(), value.trim()) {
                    ("prefers-color-scheme", "dark") => {
                        branch.color_scheme = Some(true);
                        true
                    }
                    ("prefers-color-scheme", "light") => {
                        branch.color_scheme = Some(false);
                        true
                    }
                    ("min-width", width) => {
                        branch.min_width = parse_media_px(width);
                        branch.min_width.is_some()
                    }
                    ("max-width", width) => {
                        branch.max_width = parse_media_px(width);
                        branch.max_width.is_some()
                    }
                    _ => false,
                },
                None => false,
            };
            branch.unknown |= !known;
            rest = &feature[close + 1..];
        } else {
            let end = rest
                .find(|c: char| c.is_ascii_whitespace() || c == '(')
                .unwrap_or(rest.len());
            match &rest[..end] {
                "and" | "only" | "all" | "screen" => {}
                "not" => branch.negated = true,
                "or" => branch.unknown = true,
                // Print, speech and every other medium a window is not.
                _ => branch.screen = false,
            }
            rest = &rest[end..];
        }
        rest = rest.trim_start();
    }
    branch
}

fn parse_media_px(v: &str) -> Option<f32> {
    if let Some(n) = v.strip_suffix("px") {
        number(n)
    } else if let Some(n) = v.strip_suffix("rem").or_else(|| v.strip_suffix("em")) {
        number(n).map(|x| x * 16.0)
    } else {
        number(v)
    }
}

fn parse_selector_list(raw: &str, depth: usize, left_out: &mut bool) -> Vec<Selector> {
    split_top_level(raw, ',')
        .into_iter()
        .filter_map(|selector| parse_selector(selector, depth, left_out))
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

fn parse_selector(raw: &str, depth: usize, left_out: &mut bool) -> Option<Selector> {
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
        let (compound, pe) = parse_compound(s, &mut pos, depth, left_out)?;
        if pe.is_some() {
            pseudo_element = pe;
        }
        parts.push((pending_comb.take(), compound));
    }

    if parts.is_empty() {
        return None;
    }
    let specificity = compute_specificity(&parts, pseudo_element.is_some());
    let retries = parts
        .iter()
        .filter(|(combinator, _)| {
            matches!(
                combinator,
                Some(Combinator::Descendant | Combinator::GeneralSibling)
            )
        })
        .count()
        >= 2;
    Some(Selector {
        parts,
        pseudo_element,
        specificity,
        retries,
    })
}

fn skip_ws_flag(bytes: &[u8], pos: &mut usize) -> bool {
    let start = *pos;
    while *pos < bytes.len() && bytes[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
    *pos > start
}

fn parse_compound(
    s: &str,
    pos: &mut usize,
    depth: usize,
    left_out: &mut bool,
) -> Option<(CompoundSelector, Option<PseudoElement>)> {
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
                if bytes.get(*pos) == Some(&b':') {
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
                    "not" | "is" | "where" => {
                        if depth >= MAX_NESTING {
                            *left_out = true;
                            return None;
                        }
                        let inner = arg
                            .map(|arg| parse_selector_list(arg, depth + 1, left_out))
                            .unwrap_or_default();
                        compound.pseudos.push(match name.as_str() {
                            "not" => PseudoClass::Not(inner),
                            "is" => PseudoClass::Is(inner),
                            _ => PseudoClass::Where(inner),
                        });
                    }
                    _ => compound.pseudos.push(PseudoClass::Unsupported),
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

struct Matching<'a> {
    doc: &'a Document,
    budget: &'a mut Budget,
    /// What a selector that [retries](Selector::retries) made of a node for
    /// one of its parts, so that it is tried there once.
    seen: HashMap<(*const Selector, usize, NodeId), bool>,
}

impl Matching<'_> {
    fn selector(&mut self, sel: &Selector, node_id: NodeId) -> bool {
        self.parts(sel, sel.parts.len() - 1, node_id)
    }

    fn parts(&mut self, sel: &Selector, idx: usize, node_id: NodeId) -> bool {
        if !sel.retries {
            return self.parts_untried(sel, idx, node_id);
        }
        let key = (std::ptr::from_ref(sel), idx, node_id);
        if let Some(&seen) = self.seen.get(&key) {
            return seen;
        }
        let matched = self.parts_untried(sel, idx, node_id);
        self.seen.insert(key, matched);
        matched
    }

    fn parts_untried(&mut self, sel: &Selector, idx: usize, node_id: NodeId) -> bool {
        let (comb, compound) = &sel.parts[idx];
        if !self.compound(compound, node_id) {
            return false;
        }
        if idx == 0 {
            return true;
        }
        let doc = self.doc;
        let nodes = &doc.nodes;
        match comb.unwrap_or(Combinator::Descendant) {
            Combinator::Child => nodes[node_id]
                .parent
                .is_some_and(|p| self.parts(sel, idx - 1, p)),
            Combinator::Descendant => {
                let mut cur = nodes[node_id].parent;
                while let Some(p) = cur {
                    if self.parts(sel, idx - 1, p) {
                        return true;
                    }
                    cur = nodes[p].parent;
                }
                false
            }
            Combinator::AdjacentSibling => nodes[node_id]
                .place
                .previous
                .is_some_and(|prev| self.parts(sel, idx - 1, prev)),
            Combinator::GeneralSibling => {
                let mut cur = nodes[node_id].place.previous;
                while let Some(prev) = cur {
                    if self.parts(sel, idx - 1, prev) {
                        return true;
                    }
                    cur = nodes[prev].place.previous;
                }
                false
            }
        }
    }

    fn compound(&mut self, comp: &CompoundSelector, node_id: NodeId) -> bool {
        let doc = self.doc;
        let Some(el) = doc.element(node_id) else {
            return false;
        };
        self.budget.spend(1)
            && comp.tag.as_ref().is_none_or(|tag| el.tag == *tag)
            && comp
                .id
                .as_ref()
                .is_none_or(|id| el.id.as_deref() == Some(id.as_str()))
            && comp.classes.iter().all(|class| el.has_class(class))
            && comp.attrs.iter().all(|attr| attr_matches(attr, el))
            && comp
                .pseudos
                .iter()
                .all(|pseudo| self.pseudo(pseudo, node_id))
    }

    fn pseudo(&mut self, pseudo: &PseudoClass, node_id: NodeId) -> bool {
        let doc = self.doc;
        let node = &doc.nodes[node_id];
        let place = node.place;
        match pseudo {
            PseudoClass::Root => node.parent.is_none(),
            PseudoClass::FirstChild => place.index == 1,
            PseudoClass::LastChild => place.last,
            PseudoClass::OnlyChild => place.index == 1 && place.last,
            PseudoClass::FirstOfType => place.index_of_type == 1,
            PseudoClass::LastOfType => place.last_of_type,
            PseudoClass::Empty => node
                .children
                .iter()
                .all(|&c| matches!(&doc.nodes[c].kind, NodeKind::Text(t) if t.is_empty())),
            PseudoClass::NthChild(a, b) => {
                let (a, diff) = (i64::from(*a), i64::from(place.index) - i64::from(*b));
                if a == 0 {
                    diff == 0
                } else {
                    diff % a == 0 && diff / a >= 0
                }
            }
            PseudoClass::Not(list) => !list.iter().any(|s| self.selector(s, node_id)),
            PseudoClass::Is(list) | PseudoClass::Where(list) => {
                list.iter().any(|s| self.selector(s, node_id))
            }
            PseudoClass::Unsupported => false,
        }
    }
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
