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
                let decoded = decode_entities(&self.input[start..self.pos], false);
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

        if !self.make_way_for(&tag) {
            return;
        }
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
        if let Some(at) = self.open_in_scope(&[tag.as_str()]) {
            self.stack.truncate(at);
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
            decode_entities(raw, true)
        } else {
            let start = self.pos;
            while self.pos < self.bytes.len()
                && !self.bytes[self.pos].is_ascii_whitespace()
                && !matches!(self.bytes[self.pos], b'>' | b'/')
            {
                self.pos += 1;
            }
            decode_entities(&self.input[start..self.pos], true)
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

    fn tag_of(&self, open: NodeId) -> &str {
        self.doc.element(open).map_or("", |el| el.tag.as_str())
    }

    /// The innermost open element that is one of `tags`, unless a nearer one
    /// bounds the scope HTML looks for them in.
    fn open_in_scope(&self, tags: &[&str]) -> Option<usize> {
        for (at, &open) in self.stack.iter().enumerate().rev() {
            let open = self.tag_of(open);
            if tags.contains(&open) {
                return Some(at);
            }
            if bounds_scope(open, tags[0]) {
                return None;
            }
        }
        None
    }

    /// Close what HTML has a `tag` start tag close, and open the table parts
    /// it implies. False for a table part outside a table, which HTML drops.
    fn make_way_for(&mut self, tag: &str) -> bool {
        if is_table_part(tag) {
            let Some(table) = self.open_in_scope(&["table"]) else {
                return false;
            };
            self.clear_to_table_parent(tag, table);
            return true;
        }
        let closed: &[&str] = match tag {
            "li" => &["li"],
            "dd" | "dt" => &["dd", "dt"],
            _ if closes_p(tag) => &["p"],
            _ => return true,
        };
        if let Some(at) = self.open_in_scope(closed) {
            self.stack.truncate(at);
        }
        if is_heading(tag)
            && let Some(&top) = self.stack.last()
            && is_heading(self.tag_of(top))
        {
            self.stack.pop();
        }
        true
    }

    /// Leave open what a `tag` of the table open at `table` is a child of,
    /// opening the `tbody`, `tr` or `colgroup` it was written without.
    fn clear_to_table_parent(&mut self, tag: &str, table: usize) {
        let parents: &[&str] = match tag {
            "td" | "th" => &["tr"],
            "tr" => &["tbody", "thead", "tfoot"],
            "col" => &["colgroup"],
            _ => {
                self.stack.truncate(table + 1);
                return;
            }
        };
        let parent = self.stack[table..]
            .iter()
            .rposition(|&open| parents.contains(&self.tag_of(open)));
        match parent {
            Some(parent) => self.stack.truncate(table + parent + 1),
            None => {
                let implied = parents[0];
                self.clear_to_table_parent(implied, table);
                self.open_element(build_element(implied.to_owned(), Vec::new(), None), false);
            }
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

fn is_table_part(tag: &str) -> bool {
    matches!(
        tag,
        "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
    )
}

fn is_heading(tag: &str) -> bool {
    matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

/// Whether a start tag ends a paragraph left open.
fn closes_p(tag: &str) -> bool {
    is_heading(tag)
        || matches!(
            tag,
            "address"
                | "article"
                | "aside"
                | "blockquote"
                | "details"
                | "div"
                | "dl"
                | "fieldset"
                | "figcaption"
                | "figure"
                | "footer"
                | "form"
                | "header"
                | "hgroup"
                | "hr"
                | "main"
                | "menu"
                | "nav"
                | "ol"
                | "p"
                | "pre"
                | "section"
                | "table"
                | "ul"
        )
}

/// Whether an open `tag` ends the scope HTML looks in for an open `wanted`:
/// a table and its parts are those of the nearest table, an item is one of
/// the nearest list, and nothing else reaches out of a table or its cell.
fn bounds_scope(tag: &str, wanted: &str) -> bool {
    if wanted == "table" || is_table_part(wanted) {
        return tag == "table";
    }
    matches!(tag, "table" | "td" | "th" | "caption")
        || match wanted {
            "li" => matches!(tag, "ul" | "ol"),
            "dd" | "dt" => tag == "dl",
            _ => false,
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

/// `input` with its character references decoded as HTML decodes them in
/// text, or in an attribute's value.
fn decode_entities(input: &str, in_attribute: bool) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        // No reference is longer than this; looking further makes a run of
        // ampersands quadratic.
        let head = &rest.as_bytes()[..rest.len().min(17)];
        let ended = head
            .iter()
            .position(|&b| b == b';')
            .and_then(|semi| Some((semi + 1, reference(&rest[..semi])?)));
        match ended.or_else(|| legacy_reference(rest, in_attribute)) {
            Some((len, decoded)) => {
                out.push(decoded);
                rest = &rest[len..];
            }
            None => out.push('&'),
        }
    }
    out.push_str(rest);
    out
}

/// What a C1 control's number stands for: HTML reads it as the byte of
/// Windows-1252 it is nearly always written for.
const WINDOWS_1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

/// The character a reference written between `&` and `;` stands for.
fn reference(name: &str) -> Option<char> {
    let Some(number) = name.strip_prefix('#') else {
        return named_reference(name);
    };
    let code = match number.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => number.parse(),
    }
    .ok()?;
    match code {
        0x80..=0x9f => Some(WINDOWS_1252[code as usize - 0x80]),
        _ => char::from_u32(code),
    }
}

/// The longest name at the start of `rest` that HTML decodes without its
/// semicolon, and its length: `amp`, `lt`, `gt`, `quot` and the names of
/// ISO 8859-1, none longer than six letters. In an attribute's value HTML
/// leaves one as written before `=`, a letter or a digit, where it is a
/// parameter of an address far more often than a reference.
fn legacy_reference(rest: &str, in_attribute: bool) -> Option<(usize, char)> {
    let (len, decoded) = (2..=rest.len().min(6)).rev().find_map(|len| {
        let name = rest.get(..len)?;
        let decoded = named_reference(name)?;
        (matches!(name, "amp" | "lt" | "gt" | "quot") || matches!(decoded, '\u{a0}'..='\u{ff}'))
            .then_some((len, decoded))
    })?;
    let joined = rest
        .as_bytes()
        .get(len)
        .is_some_and(|&next| next == b'=' || next.is_ascii_alphanumeric());
    (!in_attribute || !joined).then_some((len, decoded))
}

/// The names of HTML 4 and, of those HTML5 added, the ones for ASCII
/// punctuation and the few symbols models write.
fn named_reference(name: &str) -> Option<char> {
    Some(match name {
        "AElig" => 'Æ',
        "Aacute" => 'Á',
        "Acirc" => 'Â',
        "Agrave" => 'À',
        "Alpha" => 'Α',
        "Aring" => 'Å',
        "Atilde" => 'Ã',
        "Auml" => 'Ä',
        "Beta" => 'Β',
        "Ccedil" => 'Ç',
        "Chi" => 'Χ',
        "Dagger" => '‡',
        "Delta" => 'Δ',
        "ETH" => 'Ð',
        "Eacute" => 'É',
        "Ecirc" => 'Ê',
        "Egrave" => 'È',
        "Epsilon" => 'Ε',
        "Eta" => 'Η',
        "Euml" => 'Ë',
        "Gamma" => 'Γ',
        "Iacute" => 'Í',
        "Icirc" => 'Î',
        "Igrave" => 'Ì',
        "Iota" => 'Ι',
        "Iuml" => 'Ï',
        "Kappa" => 'Κ',
        "Lambda" => 'Λ',
        "Mu" => 'Μ',
        "Ntilde" => 'Ñ',
        "Nu" => 'Ν',
        "OElig" => 'Œ',
        "Oacute" => 'Ó',
        "Ocirc" => 'Ô',
        "Ograve" => 'Ò',
        "Omega" => 'Ω',
        "Omicron" => 'Ο',
        "Oslash" => 'Ø',
        "Otilde" => 'Õ',
        "Ouml" => 'Ö',
        "Phi" => 'Φ',
        "Pi" => 'Π',
        "Prime" => '″',
        "Psi" => 'Ψ',
        "Rho" => 'Ρ',
        "Scaron" => 'Š',
        "Sigma" => 'Σ',
        "THORN" => 'Þ',
        "Tau" => 'Τ',
        "Theta" => 'Θ',
        "Uacute" => 'Ú',
        "Ucirc" => 'Û',
        "Ugrave" => 'Ù',
        "Upsilon" => 'Υ',
        "Uuml" => 'Ü',
        "Xi" => 'Ξ',
        "Yacute" => 'Ý',
        "Yuml" => 'Ÿ',
        "Zeta" => 'Ζ',
        "aacute" => 'á',
        "acirc" => 'â',
        "acute" => '´',
        "aelig" => 'æ',
        "agrave" => 'à',
        "alefsym" => 'ℵ',
        "alpha" => 'α',
        "amp" => '&',
        "and" => '∧',
        "ang" => '∠',
        "apos" => '\'',
        "approx" => '≈',
        "aring" => 'å',
        "ast" => '*',
        "asymp" => '≈',
        "atilde" => 'ã',
        "auml" => 'ä',
        "bdquo" => '„',
        "beta" => 'β',
        "brvbar" => '¦',
        "bsol" => '\\',
        "bull" => '•',
        "cap" => '∩',
        "ccedil" => 'ç',
        "cedil" => '¸',
        "cent" => '¢',
        "check" => '✓',
        "checkmark" => '✓',
        "chi" => 'χ',
        "circ" => 'ˆ',
        "clubs" => '♣',
        "colon" => ':',
        "comma" => ',',
        "commat" => '@',
        "cong" => '≅',
        "copy" => '©',
        "crarr" => '↵',
        "cross" => '✗',
        "cup" => '∪',
        "curren" => '¤',
        "dArr" => '⇓',
        "dagger" => '†',
        "darr" => '↓',
        "deg" => '°',
        "delta" => 'δ',
        "diams" => '♦',
        "divide" => '÷',
        "dollar" => '$',
        "eacute" => 'é',
        "ecirc" => 'ê',
        "egrave" => 'è',
        "empty" => '∅',
        "emsp" => '\u{2003}',
        "ensp" => '\u{2002}',
        "epsilon" => 'ε',
        "equals" => '=',
        "equiv" => '≡',
        "eta" => 'η',
        "eth" => 'ð',
        "euml" => 'ë',
        "euro" => '€',
        "excl" => '!',
        "exist" => '∃',
        "fnof" => 'ƒ',
        "forall" => '∀',
        "frac12" => '½',
        "frac14" => '¼',
        "frac34" => '¾',
        "frasl" => '⁄',
        "gamma" => 'γ',
        "ge" => '≥',
        "grave" => '`',
        "gt" => '>',
        "hArr" => '⇔',
        "harr" => '↔',
        "hearts" => '♥',
        "hellip" => '…',
        "iacute" => 'í',
        "icirc" => 'î',
        "iexcl" => '¡',
        "igrave" => 'ì',
        "image" => 'ℑ',
        "infin" => '∞',
        "int" => '∫',
        "iota" => 'ι',
        "iquest" => '¿',
        "isin" => '∈',
        "iuml" => 'ï',
        "kappa" => 'κ',
        "lArr" => '⇐',
        "lambda" => 'λ',
        "lang" => '〈',
        "laquo" => '«',
        "larr" => '←',
        "lbrace" => '{',
        "lbrack" => '[',
        "lceil" => '⌈',
        "lcub" => '{',
        "ldquo" => '“',
        "le" => '≤',
        "lfloor" => '⌊',
        "lowast" => '∗',
        "lowbar" => '_',
        "loz" => '◊',
        "lpar" => '(',
        "lrm" => '\u{200e}',
        "lsaquo" => '‹',
        "lsqb" => '[',
        "lsquo" => '‘',
        "lt" => '<',
        "macr" => '¯',
        "mdash" => '—',
        "micro" => 'µ',
        "middot" => '·',
        "minus" => '−',
        "mu" => 'μ',
        "nabla" => '∇',
        "nbsp" => '\u{a0}',
        "ndash" => '–',
        "ne" => '≠',
        "ni" => '∋',
        "not" => '¬',
        "notin" => '∉',
        "nsub" => '⊄',
        "ntilde" => 'ñ',
        "nu" => 'ν',
        "num" => '#',
        "oacute" => 'ó',
        "ocirc" => 'ô',
        "oelig" => 'œ',
        "ograve" => 'ò',
        "oline" => '‾',
        "omega" => 'ω',
        "omicron" => 'ο',
        "oplus" => '⊕',
        "or" => '∨',
        "ordf" => 'ª',
        "ordm" => 'º',
        "oslash" => 'ø',
        "otilde" => 'õ',
        "otimes" => '⊗',
        "ouml" => 'ö',
        "para" => '¶',
        "part" => '∂',
        "percnt" => '%',
        "period" => '.',
        "permil" => '‰',
        "perp" => '⊥',
        "phi" => 'φ',
        "pi" => 'π',
        "piv" => 'ϖ',
        "plus" => '+',
        "plusmn" => '±',
        "pound" => '£',
        "prime" => '′',
        "prod" => '∏',
        "prop" => '∝',
        "psi" => 'ψ',
        "quest" => '?',
        "quot" => '"',
        "rArr" => '⇒',
        "radic" => '√',
        "rang" => '〉',
        "raquo" => '»',
        "rarr" => '→',
        "rbrace" => '}',
        "rbrack" => ']',
        "rceil" => '⌉',
        "rcub" => '}',
        "rdquo" => '”',
        "real" => 'ℜ',
        "reg" => '®',
        "rfloor" => '⌋',
        "rho" => 'ρ',
        "rlm" => '\u{200f}',
        "rpar" => ')',
        "rsaquo" => '›',
        "rsqb" => ']',
        "rsquo" => '’',
        "sbquo" => '‚',
        "scaron" => 'š',
        "sdot" => '⋅',
        "sect" => '§',
        "semi" => ';',
        "shy" => '\u{ad}',
        "sigma" => 'σ',
        "sigmaf" => 'ς',
        "sim" => '∼',
        "sol" => '/',
        "spades" => '♠',
        "star" => '☆',
        "starf" => '★',
        "sub" => '⊂',
        "sube" => '⊆',
        "sum" => '∑',
        "sup" => '⊃',
        "sup1" => '¹',
        "sup2" => '²',
        "sup3" => '³',
        "supe" => '⊇',
        "szlig" => 'ß',
        "tau" => 'τ',
        "there4" => '∴',
        "theta" => 'θ',
        "thetasym" => 'ϑ',
        "thinsp" => '\u{2009}',
        "thorn" => 'þ',
        "tilde" => '˜',
        "times" => '×',
        "trade" => '™',
        "uArr" => '⇑',
        "uacute" => 'ú',
        "uarr" => '↑',
        "ucirc" => 'û',
        "ugrave" => 'ù',
        "uml" => '¨',
        "upsih" => 'ϒ',
        "upsilon" => 'υ',
        "uuml" => 'ü',
        "verbar" => '|',
        "vert" => '|',
        "weierp" => '℘',
        "xi" => 'ξ',
        "yacute" => 'ý',
        "yen" => '¥',
        "yuml" => 'ÿ',
        "zeta" => 'ζ',
        "zwj" => '\u{200d}',
        "zwnj" => '\u{200c}',
        _ => return None,
    })
}
