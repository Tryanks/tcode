//! Tolerant, linear-time static HTML parser producing an arena DOM.
//!
//! Designed for self-contained visual cards: `<style>` blocks are extracted
//! into `stylesheets`, inline `<svg>` subtrees keep their raw XML verbatim for
//! SVG rasterization, and `<script>` / `<template>` / `<iframe>` / `<object>`
//! subtrees are discarded safely.

pub(crate) type NodeId = usize;

/// The most nodes a document is drawn with; [`Document::left_out`] says so
/// when there were more.
pub(crate) const MAX_NODES: usize = 4096;
/// The deepest an element nests; [`Document::flattened`] says so when one
/// was written deeper and became a sibling of what should have held it.
pub(crate) const MAX_DEPTH: usize = 64;

#[derive(Debug, Default)]
pub(crate) struct Document {
    pub(crate) nodes: Vec<Node>,
    pub(crate) roots: Vec<NodeId>,
    pub(crate) stylesheets: Vec<String>,
    pub(crate) left_out: bool,
    pub(crate) flattened: bool,
}

#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) parent: Option<NodeId>,
    pub(crate) children: Vec<NodeId>,
    pub(crate) kind: NodeKind,
    pub(crate) place: Place,
}

/// Where an element stands among its element siblings, counted once when the
/// document is parsed: counting per selector is quadratic in a long run of
/// siblings.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Place {
    pub(crate) previous: Option<NodeId>,
    /// 1-based among element siblings, and among those of the same tag.
    pub(crate) index: u32,
    pub(crate) index_of_type: u32,
    pub(crate) last: bool,
    pub(crate) last_of_type: bool,
}

#[derive(Debug)]
pub(crate) enum NodeKind {
    Element(ElementData),
    Text(String),
}

#[derive(Debug)]
pub(crate) struct ElementData {
    pub(crate) tag: String,
    pub(crate) id: Option<String>,
    pub(crate) classes: Vec<String>,
    pub(crate) inline_style: Option<String>,
    pub(crate) attrs: Vec<(String, String)>,
    /// Preserved raw `<svg ...>...</svg>` XML when `tag == "svg"`.
    pub(crate) raw_svg: Option<String>,
}

impl ElementData {
    pub(crate) fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn has_class(&self, class: &str) -> bool {
        self.classes.iter().any(|c| c == class)
    }
}

impl Document {
    pub(crate) fn parse(input: &str) -> Self {
        let mut parser = Parser::new(input);
        parser.parse();
        parser.doc.place_elements();
        parser.doc
    }

    pub(crate) fn element(&self, id: NodeId) -> Option<&ElementData> {
        match &self.nodes.get(id)?.kind {
            NodeKind::Element(el) => Some(el),
            NodeKind::Text(_) => None,
        }
    }

    fn place_elements(&mut self) {
        let mut places = vec![Place::default(); self.nodes.len()];
        let mut of_type: std::collections::HashMap<&str, (u32, NodeId)> = Default::default();
        let families = std::iter::once(&self.roots).chain(self.nodes.iter().map(|n| &n.children));
        for siblings in families {
            of_type.clear();
            let mut previous = None;
            let mut index = 0;
            for &id in siblings {
                let NodeKind::Element(el) = &self.nodes[id].kind else {
                    continue;
                };
                index += 1;
                let same = of_type.entry(el.tag.as_str()).or_insert((0, id));
                *same = (same.0 + 1, id);
                places[id] = Place {
                    previous,
                    index,
                    index_of_type: same.0,
                    last: false,
                    last_of_type: false,
                };
                previous = Some(id);
            }
            if let Some(last) = previous {
                places[last].last = true;
            }
            for (_, last) in of_type.values() {
                places[*last].last_of_type = true;
            }
        }
        drop(of_type);
        for (node, place) in self.nodes.iter_mut().zip(places) {
            node.place = place;
        }
    }
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
    doc: Document,
    stack: Vec<NodeId>,
    in_head: bool,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
            doc: Document::default(),
            stack: Vec::new(),
            in_head: false,
        }
    }

    fn parse(&mut self) {
        while self.pos < self.bytes.len() {
            if self.bytes[self.pos] == b'<' {
                if self.starts_with("<!--") {
                    self.skip_comment();
                } else if self.starts_with_ignore_ascii_case("<!doctype")
                    || self.starts_with("<!")
                    || self.starts_with("<?")
                {
                    self.skip_until_gt();
                } else if self.starts_with("</") {
                    self.parse_end_tag();
                } else if self
                    .bytes
                    .get(self.pos + 1)
                    .is_some_and(u8::is_ascii_alphabetic)
                {
                    self.parse_start_tag();
                } else {
                    self.push_text("<");
                    self.pos += 1;
                }
            } else {
                let start = self.pos;
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'<' {
                    self.pos += 1;
                }
                let decoded = decode_entities(&self.input[start..self.pos]);
                // Text is not head content: it ends a head left open.
                self.in_head &= decoded.trim().is_empty();
                if !self.in_head && !decoded.is_empty() {
                    self.push_text(&decoded);
                }
            }
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        self.input[self.pos..].starts_with(s)
    }

    fn starts_with_ignore_ascii_case(&self, s: &str) -> bool {
        self.bytes
            .get(self.pos..self.pos + s.len())
            .is_some_and(|slice| slice.eq_ignore_ascii_case(s.as_bytes()))
    }

    fn skip_comment(&mut self) {
        self.pos += 4;
        if let Some(end) = self.input[self.pos..].find("-->") {
            self.pos += end + 3;
        } else {
            self.pos = self.bytes.len();
        }
    }

    fn skip_until_gt(&mut self) {
        if let Some(end) = self.input[self.pos..].find('>') {
            self.pos += end + 1;
        } else {
            self.pos = self.bytes.len();
        }
    }

    fn parse_start_tag(&mut self) {
        let tag_start = self.pos;
        self.pos += 1; // '<'
        let name_start = self.pos;
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_alphanumeric()
                || matches!(self.bytes[self.pos], b'-' | b'_' | b':'))
        {
            self.pos += 1;
        }
        let tag = self.input[name_start..self.pos].to_ascii_lowercase();
        let (attrs, self_closing) = self.parse_attributes();

        if tag == "head" {
            if !self_closing {
                self.in_head = true;
            }
            return;
        }
        // Only metadata is head content: any other element ends a head left
        // open, as a browser's parser does, instead of taking the body with it.
        self.in_head &= matches!(
            tag.as_str(),
            "base" | "link" | "meta" | "script" | "style" | "template" | "title"
        );
        if matches!(tag.as_str(), "html" | "body") {
            // Preserve classes / inline styles on `body` or `html` only if present by wrapping,
            // otherwise treat transparently so fragment & full-document inputs render identically.
            if !self_closing
                && attrs
                    .iter()
                    .any(|(k, _)| matches!(k.as_str(), "style" | "class" | "id"))
            {
                let el = build_element(tag, attrs, None);
                self.open_element(el, false);
            }
            return;
        }
        if tag == "style" {
            if !self_closing {
                let css = self.read_raw_text("style");
                if !css.trim().is_empty() {
                    self.doc.stylesheets.push(css);
                }
            }
            return;
        }
        if matches!(
            tag.as_str(),
            "script" | "template" | "iframe" | "object" | "title"
        ) {
            if !self_closing {
                let _ = self.read_raw_text(&tag);
            }
            return;
        }
        if self.in_head {
            return;
        }
        if tag == "svg" {
            let raw_svg = if self_closing {
                self.input[tag_start..self.pos].to_string()
            } else {
                let end = self.read_svg_subtree_end();
                self.input[tag_start..end].to_string()
            };
            let el = build_element(tag, attrs, Some(raw_svg));
            self.open_element(el, true);
            return;
        }

        self.auto_close_for(&tag);
        let void = self_closing || is_void_tag(&tag);
        let el = build_element(tag, attrs, None);
        self.open_element(el, void);
    }

    fn parse_end_tag(&mut self) {
        self.pos += 2; // "</"
        let name_start = self.pos;
        while self.pos < self.bytes.len()
            && !self.bytes[self.pos].is_ascii_whitespace()
            && self.bytes[self.pos] != b'>'
        {
            self.pos += 1;
        }
        let tag = self.input[name_start..self.pos].to_ascii_lowercase();
        self.skip_until_gt();

        if tag == "head" {
            self.in_head = false;
            return;
        }
        if let Some(pos) = self
            .stack
            .iter()
            .rposition(|&id| self.doc.element(id).is_some_and(|el| el.tag == tag))
        {
            self.stack.truncate(pos);
        }
    }

    fn parse_attributes(&mut self) -> (Vec<(String, String)>, bool) {
        let mut attrs = Vec::new();
        let mut self_closing = false;
        while self.pos < self.bytes.len() {
            self.skip_ws();
            if self.pos >= self.bytes.len() {
                break;
            }
            match self.bytes[self.pos] {
                b'>' => {
                    self.pos += 1;
                    break;
                }
                b'/' => {
                    self.pos += 1;
                    if self.bytes.get(self.pos) == Some(&b'>') {
                        self_closing = true;
                        self.pos += 1;
                        break;
                    }
                }
                _ => {
                    let key_start = self.pos;
                    while self.pos < self.bytes.len()
                        && !self.bytes[self.pos].is_ascii_whitespace()
                        && !matches!(self.bytes[self.pos], b'=' | b'>' | b'/')
                    {
                        self.pos += 1;
                    }
                    if self.pos == key_start {
                        self.pos += 1;
                        continue;
                    }
                    let key = self.input[key_start..self.pos].to_ascii_lowercase();
                    self.skip_ws();
                    let val = if self.bytes.get(self.pos) == Some(&b'=') {
                        self.pos += 1;
                        self.skip_ws();
                        self.parse_attr_value()
                    } else {
                        String::new()
                    };
                    attrs.push((key, val));
                }
            }
        }
        (attrs, self_closing)
    }

    fn parse_attr_value(&mut self) -> String {
        if self.pos >= self.bytes.len() {
            return String::new();
        }
        let quote = self.bytes[self.pos];
        if quote == b'"' || quote == b'\'' {
            self.pos += 1;
            let start = self.pos;
            while self.pos < self.bytes.len() && self.bytes[self.pos] != quote {
                self.pos += 1;
            }
            let raw = &self.input[start..self.pos];
            if self.pos < self.bytes.len() {
                self.pos += 1;
            }
            decode_entities(raw)
        } else {
            let start = self.pos;
            while self.pos < self.bytes.len()
                && !self.bytes[self.pos].is_ascii_whitespace()
                && !matches!(self.bytes[self.pos], b'>' | b'/')
            {
                self.pos += 1;
            }
            decode_entities(&self.input[start..self.pos])
        }
    }

    fn read_raw_text(&mut self, tag: &str) -> String {
        let start = self.pos;
        let close = format!("</{tag}");
        let mut cursor = self.pos;
        while cursor < self.bytes.len() {
            if let Some(rel) = find_ascii_case_insensitive(&self.input[cursor..], &close) {
                let end_start = cursor + rel;
                let after = end_start + close.len();
                let valid_end = self
                    .bytes
                    .get(after)
                    .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>');
                if valid_end {
                    let content = self.input[start..end_start].to_string();
                    self.pos = after;
                    self.skip_until_gt();
                    return content;
                }
                cursor = after;
            } else {
                break;
            }
        }
        self.pos = self.bytes.len();
        self.input[start..].to_string()
    }

    fn read_svg_subtree_end(&mut self) -> usize {
        let mut depth = 1usize;
        while self.pos < self.bytes.len() && depth > 0 {
            if self.bytes[self.pos] == b'<' {
                if self.starts_with("<!--") {
                    self.skip_comment();
                } else if self.starts_with_ignore_ascii_case("</svg")
                    && self
                        .bytes
                        .get(self.pos + 5)
                        .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
                {
                    self.skip_until_gt();
                    depth -= 1;
                } else if self.starts_with_ignore_ascii_case("<svg")
                    && self
                        .bytes
                        .get(self.pos + 4)
                        .is_some_and(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'/'))
                {
                    self.pos += 4;
                    let (_, self_closing) = self.parse_attributes();
                    if !self_closing {
                        depth += 1;
                    }
                } else {
                    self.pos += 1;
                }
            } else {
                self.pos += 1;
            }
        }
        self.pos
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn auto_close_for(&mut self, incoming: &str) {
        let Some(&top) = self.stack.last() else {
            return;
        };
        let Some(top_tag) = self.doc.element(top).map(|el| el.tag.as_str()) else {
            return;
        };
        let should_close = match top_tag {
            "p" => matches!(
                incoming,
                "p" | "div"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "main"
                    | "nav"
                    | "aside"
                    | "ul"
                    | "ol"
                    | "dl"
                    | "table"
                    | "pre"
                    | "blockquote"
                    | "hr"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
            ),
            "li" => incoming == "li",
            "dt" | "dd" => matches!(incoming, "dt" | "dd"),
            "td" | "th" => matches!(incoming, "td" | "th" | "tr"),
            "tr" => incoming == "tr",
            "thead" | "tbody" | "tfoot" => matches!(incoming, "thead" | "tbody" | "tfoot"),
            _ => false,
        };
        if should_close {
            self.stack.pop();
        }
    }

    fn open_element(&mut self, el: ElementData, void: bool) {
        if self.doc.nodes.len() >= MAX_NODES {
            self.doc.left_out = true;
            return;
        }
        let parent = self.stack.last().copied();
        let id = self.doc.nodes.len();
        self.doc.nodes.push(Node {
            parent,
            children: Vec::new(),
            kind: NodeKind::Element(el),
            place: Place::default(),
        });
        match parent {
            Some(p) => self.doc.nodes[p].children.push(id),
            None => self.doc.roots.push(id),
        }
        if !void {
            if self.stack.len() < MAX_DEPTH {
                self.stack.push(id);
            } else {
                self.doc.flattened = true;
            }
        }
    }

    fn push_text(&mut self, text: &str) {
        let parent = self.stack.last().copied();
        if parent.is_none() && text.trim().is_empty() {
            return;
        }
        if self.doc.nodes.len() >= MAX_NODES {
            self.doc.left_out = true;
            return;
        }
        // Coalesce adjacent text nodes under the same parent.
        let last_sibling = match parent {
            Some(p) => self.doc.nodes[p].children.last().copied(),
            None => self.doc.roots.last().copied(),
        };
        if let Some(last) = last_sibling
            && let NodeKind::Text(existing) = &mut self.doc.nodes[last].kind
        {
            existing.push_str(text);
            return;
        }
        let id = self.doc.nodes.len();
        self.doc.nodes.push(Node {
            parent,
            children: Vec::new(),
            kind: NodeKind::Text(text.to_string()),
            place: Place::default(),
        });
        match parent {
            Some(p) => self.doc.nodes[p].children.push(id),
            None => self.doc.roots.push(id),
        }
    }
}

fn build_element(
    tag: String,
    attrs: Vec<(String, String)>,
    raw_svg: Option<String>,
) -> ElementData {
    let mut id = None;
    let mut classes = Vec::new();
    let mut inline_style = None;
    for (k, v) in &attrs {
        match k.as_str() {
            "id" if !v.is_empty() => id = Some(v.clone()),
            "class" => {
                classes.extend(v.split_whitespace().map(str::to_owned));
            }
            "style" if !v.is_empty() => inline_style = Some(v.clone()),
            _ => {}
        }
    }
    ElementData {
        tag,
        id,
        classes,
        inline_style,
        attrs,
        raw_svg,
    }
}

fn is_void_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

pub(crate) fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        // No entity is longer than this; looking further makes a run of
        // ampersands quadratic.
        let name = &rest.as_bytes()[..rest.len().min(17)];
        if let Some(semi) = name.iter().position(|&b| b == b';') {
            let entity = &rest[..semi];
            if let Some(decoded) = decode_single_entity(entity) {
                out.push_str(decoded.as_str());
                rest = &rest[semi + 1..];
                continue;
            }
        }
        out.push('&');
    }
    out.push_str(rest);
    out
}

fn decode_single_entity(entity: &str) -> Option<String> {
    if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        let code = u32::from_str_radix(hex, 16).ok()?;
        return char::from_u32(code).map(|c| c.to_string());
    }
    if let Some(dec) = entity.strip_prefix('#') {
        let code = dec.parse::<u32>().ok()?;
        return char::from_u32(code).map(|c| c.to_string());
    }
    let s = match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{00a0}",
        "ndash" => "–",
        "mdash" => "—",
        "hellip" => "…",
        "times" => "×",
        "divide" => "÷",
        "plusmn" => "±",
        "minus" => "−",
        "rarr" => "→",
        "larr" => "←",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "bull" => "•",
        "middot" => "·",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "deg" => "°",
        "check" => "✓",
        "cross" => "✗",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "laquo" => "«",
        "raquo" => "»",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "le" => "≤",
        "ge" => "≥",
        "ne" => "≠",
        "approx" => "≈",
        "infin" => "∞",
        _ => return None,
    };
    Some(s.to_string())
}
