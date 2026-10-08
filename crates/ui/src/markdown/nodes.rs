//! Markdown render IR.
//!
//! Selection storage is adapted from gpui-component's Apache-2.0 text node
//! implementation; parsing and the node shapes themselves are tcode-owned.

use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

use gpui::{SharedString, SharedUri};

use super::inline::InlineState;

/// A text offset in a leaf of a root block: its index among the block's text
/// leaves and, in a code block, the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TextPosition {
    pub(super) block: usize,
    pub(super) leaf: usize,
    pub(super) line: usize,
    pub(super) offset: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BlockNode {
    Root {
        children: Vec<BlockNode>,
    },
    Paragraph(Paragraph),
    Heading {
        level: u8,
        children: Paragraph,
    },
    Blockquote {
        children: Vec<BlockNode>,
    },
    List {
        children: Vec<BlockNode>,
        ordered: bool,
        /// First item number of an ordered list (CommonMark `start`).
        start: u32,
    },
    ListItem {
        children: Vec<BlockNode>,
        spread: bool,
        checked: Option<bool>,
    },
    CodeBlock(CodeBlock),
    Table(Table),
    HorizontalRule,
    Unknown,
}

impl BlockNode {
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Root { children, .. }
            | Self::Blockquote { children, .. }
            | Self::List { children, .. }
            | Self::ListItem { children, .. } => children
                .iter()
                .map(Self::text)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Paragraph(paragraph) => paragraph.text(),
            Self::Heading { children, .. } => children.text(),
            Self::CodeBlock(code) => code
                .formula
                .text()
                .unwrap_or_else(|| rendered_code_text(&code.code)),
            Self::Table(table) => table
                .children
                .iter()
                .map(|row| {
                    row.children
                        .iter()
                        .map(|cell| cell.children.text())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::HorizontalRule | Self::Unknown => String::new(),
        }
    }

    pub(super) fn selected_text(&self) -> String {
        // Paint-time ranges are authoritative at the two drag boundaries, but
        // an intervening leaf may be clipped or virtualized and have no range.
        // Recover those leaves from the render IR in document order.
        let mut leaves = Vec::new();
        self.collect_text_leaves(&mut leaves);
        let selections = leaves
            .iter()
            .map(|leaf| leaf.selected_leaf_text())
            .collect::<Vec<_>>();
        let Some(first) = selections.iter().position(|text| !text.is_empty()) else {
            return String::new();
        };
        let last = selections
            .iter()
            .rposition(|text| !text.is_empty())
            .unwrap_or(first);

        (first..=last)
            .map(|ix| {
                if ix == first || ix == last {
                    selections[ix].clone()
                } else {
                    leaves[ix].text()
                }
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The position in this block of `state`, painted at `offset`, when it is
    /// the whole text of a paragraph, heading or code line of the block.
    pub(super) fn position_of(
        &self,
        block: usize,
        state: &Arc<Mutex<InlineState>>,
        offset: usize,
    ) -> Option<TextPosition> {
        let mut leaves = Vec::new();
        self.collect_text_leaves(&mut leaves);
        leaves.iter().enumerate().find_map(|(leaf, node)| {
            let (line, offset) = match node {
                Self::Paragraph(paragraph)
                | Self::Heading {
                    children: paragraph,
                    ..
                } => (0, paragraph.offset_of(state, offset)?),
                Self::CodeBlock(code) => {
                    if let Some(offset) = code.formula.offset_of(state, offset) {
                        (0, offset)
                    } else {
                        (
                            code.line_states
                                .lock()
                                .ok()?
                                .iter()
                                .position(|line| Arc::ptr_eq(line, state))?,
                            offset,
                        )
                    }
                }
                _ => return None,
            };
            Some(TextPosition {
                block,
                leaf,
                line,
                offset,
            })
        })
    }

    /// A copy of this block with fresh selection state holding exactly the
    /// text from `start` to `end`; a missing end is the block's own. The
    /// copy's text does not depend on which of its leaves were painted.
    pub(super) fn selected_between(
        &self,
        start: Option<TextPosition>,
        end: Option<TextPosition>,
    ) -> BlockNode {
        let detached = self.detached();
        let mut leaves = Vec::new();
        detached.collect_text_leaves(&mut leaves);
        for (ix, leaf) in leaves.into_iter().enumerate() {
            if start.is_some_and(|start| ix < start.leaf) || end.is_some_and(|end| ix > end.leaf) {
                continue;
            }
            let at = |position: Option<TextPosition>| {
                position
                    .filter(|position| position.leaf == ix)
                    .map(|position| (position.line, position.offset))
            };
            leaf.select_leaf(at(start), at(end));
        }
        detached
    }

    fn detached(&self) -> BlockNode {
        let children = |children: &[BlockNode]| children.iter().map(Self::detached).collect();
        match self {
            Self::Root { children: nodes } => Self::Root {
                children: children(nodes),
            },
            Self::Blockquote { children: nodes } => Self::Blockquote {
                children: children(nodes),
            },
            Self::List {
                children: nodes,
                ordered,
                start,
            } => Self::List {
                children: children(nodes),
                ordered: *ordered,
                start: *start,
            },
            Self::ListItem {
                children: nodes,
                spread,
                checked,
            } => Self::ListItem {
                children: children(nodes),
                spread: *spread,
                checked: *checked,
            },
            Self::Paragraph(paragraph) => Self::Paragraph(paragraph.detached()),
            Self::Heading { level, children } => Self::Heading {
                level: *level,
                children: children.detached(),
            },
            Self::CodeBlock(code) => Self::CodeBlock(CodeBlock {
                code: code
                    .formula
                    .text()
                    .map(Into::into)
                    .unwrap_or_else(|| code.code.clone()),
                formula: Default::default(),
                lang: code.lang.clone(),
                line_states: Arc::new(Mutex::new(
                    code.formula
                        .text()
                        .unwrap_or_else(|| rendered_code_text(&code.code))
                        .split('\n')
                        .map(|line| InlineState::shared(line.to_string().into()))
                        .collect(),
                )),
            }),
            Self::Table(table) => Self::Table(Table {
                children: table
                    .children
                    .iter()
                    .map(|row| TableRow {
                        children: row
                            .children
                            .iter()
                            .map(|cell| TableCell {
                                children: cell.children.detached(),
                            })
                            .collect(),
                    })
                    .collect(),
                column_aligns: table.column_aligns.clone(),
            }),
            Self::HorizontalRule => Self::HorizontalRule,
            Self::Unknown => Self::Unknown,
        }
    }

    /// Select a detached leaf from `(line, offset)` `from` to `to`, or from
    /// its start or to its end when they are missing.
    fn select_leaf(&self, from: Option<(usize, usize)>, to: Option<(usize, usize)>) {
        match self {
            Self::Paragraph(paragraph)
            | Self::Heading {
                children: paragraph,
                ..
            } => {
                select_inline(&paragraph.state, from.map(|from| from.1), to.map(|to| to.1));
            }
            Self::CodeBlock(code) => {
                let Ok(states) = code.line_states.lock() else {
                    return;
                };
                let (first, last) = (
                    from.map_or(0, |from| from.0),
                    to.map_or(states.len(), |to| to.0),
                );
                for (ix, state) in states.iter().enumerate() {
                    if (first..=last).contains(&ix) {
                        select_inline(
                            state,
                            from.filter(|from| from.0 == ix).map(|from| from.1),
                            to.filter(|to| to.0 == ix).map(|to| to.1),
                        );
                    }
                }
            }
            Self::Table(table) => {
                for cell in table.children.iter().flat_map(|row| &row.children) {
                    select_inline(&cell.children.state, None, None);
                }
            }
            _ => {}
        }
    }

    fn collect_text_leaves<'a>(&'a self, leaves: &mut Vec<&'a BlockNode>) {
        match self {
            Self::Root { children, .. }
            | Self::Blockquote { children, .. }
            | Self::List { children, .. }
            | Self::ListItem { children, .. } => {
                for child in children {
                    child.collect_text_leaves(leaves);
                }
            }
            Self::Paragraph(_) | Self::Heading { .. } | Self::CodeBlock(_) | Self::Table(_) => {
                leaves.push(self)
            }
            Self::HorizontalRule | Self::Unknown => {}
        }
    }

    fn selected_leaf_text(&self) -> String {
        match self {
            Self::Paragraph(paragraph) => paragraph.selected_text(),
            Self::Heading { children, .. } => children.selected_text(),
            Self::CodeBlock(code) => code.selected_text(),
            Self::Table(table) => table
                .children
                .iter()
                .filter_map(|row| {
                    let text = row
                        .children
                        .iter()
                        .filter_map(|cell| {
                            let text = cell.children.selected_text();
                            (!text.is_empty()).then_some(text)
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    (!text.is_empty()).then_some(text)
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Root { .. }
            | Self::Blockquote { .. }
            | Self::List { .. }
            | Self::ListItem { .. }
            | Self::HorizontalRule
            | Self::Unknown => String::new(),
        }
    }

    pub(super) fn clear_selection(&self) {
        match self {
            Self::Root { children, .. }
            | Self::Blockquote { children, .. }
            | Self::List { children, .. }
            | Self::ListItem { children, .. } => {
                children.iter().for_each(Self::clear_selection);
            }
            Self::Paragraph(paragraph) => paragraph.clear_selection(),
            Self::Heading { children, .. } => children.clear_selection(),
            Self::CodeBlock(code) => code.clear_selection(),
            Self::Table(table) => {
                for row in &table.children {
                    for cell in &row.children {
                        cell.children.clear_selection();
                    }
                }
            }
            Self::HorizontalRule | Self::Unknown => {}
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct LinkMark {
    pub(crate) url: SharedString,
    pub(crate) title: Option<SharedString>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct TextMark {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) strikethrough: bool,
    pub(crate) code: bool,
    pub(crate) link: Option<LinkMark>,
}

impl TextMark {
    pub(crate) fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub(crate) fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub(crate) fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    pub(crate) fn code(mut self) -> Self {
        self.code = true;
        self
    }

    pub(crate) fn link(mut self, link: impl Into<LinkMark>) -> Self {
        self.link = Some(link.into());
        self
    }

    pub(crate) fn merge(&mut self, other: TextMark) {
        self.bold |= other.bold;
        self.italic |= other.italic;
        self.strikethrough |= other.strikethrough;
        self.code |= other.code;
        if other.link.is_some() {
            self.link = other.link;
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ImageNode {
    pub(crate) url: SharedUri,
    pub(crate) link: Option<LinkMark>,
    pub(crate) title: Option<SharedString>,
    pub(crate) alt: Option<SharedString>,
}

impl ImageNode {
    pub(crate) fn title(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| self.alt.clone().unwrap_or_default())
            .to_string()
    }
}

impl PartialEq for ImageNode {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
            && self.link == other.link
            && self.title == other.title
            && self.alt == other.alt
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct InlineNode {
    pub(crate) text: SharedString,
    pub(crate) image: Option<ImageNode>,
    pub(super) math: Option<super::math_parse::MathSpan>,
    pub(crate) marks: Vec<(Range<usize>, TextMark)>,
    pub(super) state: Arc<Mutex<InlineState>>,
}

impl PartialEq for InlineNode {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.image == other.image
            && self.math == other.math
            && self.marks == other.marks
    }
}

impl InlineNode {
    fn rendered_text(&self) -> String {
        self.math
            .as_ref()
            .and_then(|math| math.selection.text())
            .unwrap_or_else(|| self.text.to_string())
    }

    pub(crate) fn new(text: impl Into<SharedString>) -> Self {
        let text = text.into();
        Self {
            state: InlineState::shared(text.clone()),
            text,
            image: None,
            math: None,
            marks: vec![],
        }
    }

    pub(crate) fn image(image: ImageNode) -> Self {
        let mut node = Self::new("");
        node.image = Some(image);
        node
    }

    pub(crate) fn marks(mut self, marks: Vec<(Range<usize>, TextMark)>) -> Self {
        self.marks = marks;
        self
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Paragraph {
    pub(crate) children: Vec<InlineNode>,
    pub(super) state: Arc<Mutex<InlineState>>,
}

impl PartialEq for Paragraph {
    fn eq(&self, other: &Self) -> bool {
        self.children == other.children
    }
}

impl Paragraph {
    fn offset_of(&self, state: &Arc<Mutex<InlineState>>, offset: usize) -> Option<usize> {
        if Arc::ptr_eq(&self.state, state) {
            return Some(offset);
        }
        let mut start = 0;
        for child in &self.children {
            if let Some(offset) = child
                .math
                .as_ref()
                .and_then(|math| math.selection.offset_of(state, offset))
            {
                return Some(start + offset);
            }
            if Arc::ptr_eq(&child.state, state) {
                return Some(start + offset);
            }
            start += child.rendered_text().len();
        }
        None
    }

    /// A copy with fresh selection state, the whole text in `state`.
    fn detached(&self) -> Self {
        Self {
            children: self
                .children
                .iter()
                .map(|child| InlineNode {
                    text: child.rendered_text().into(),
                    state: InlineState::shared(child.rendered_text().into()),
                    math: None,
                    ..child.clone()
                })
                .collect(),
            state: InlineState::shared(self.text().into()),
        }
    }

    pub(crate) fn text(&self) -> String {
        self.children
            .iter()
            .map(|node| node.rendered_text())
            .collect()
    }

    pub(super) fn selected_text(&self) -> String {
        let mut text = String::new();
        for child in &self.children {
            if let Some(selected) = child
                .math
                .as_ref()
                .and_then(|math| math.selection.selected_text())
            {
                text.push_str(&selected);
            } else {
                append_selection(&mut text, &child.state);
            }
        }
        append_selection(&mut text, &self.state);
        text
    }

    pub(super) fn clear_selection(&self) {
        for child in &self.children {
            if let Some(math) = &child.math {
                math.selection.clear();
            }
            clear_inline_selection(&child.state);
        }
        clear_inline_selection(&self.state);
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CodeBlock {
    pub(super) formula: super::math_layout::FormulaSelection,
    pub(crate) code: SharedString,
    pub(crate) lang: Option<SharedString>,
    pub(super) line_states: Arc<Mutex<Vec<Arc<Mutex<InlineState>>>>>,
}

impl PartialEq for CodeBlock {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.lang == other.lang
    }
}

impl CodeBlock {
    /// The selection states of `lines[span]`, the lines one list item paints.
    pub(super) fn states_for_lines(
        &self,
        lines: &[&str],
        span: Range<usize>,
    ) -> Vec<Arc<Mutex<InlineState>>> {
        let Ok(mut states) = self.line_states.lock() else {
            return lines[span]
                .iter()
                .map(|line| InlineState::shared((*line).into()))
                .collect();
        };
        if states.len() != lines.len() {
            *states = lines
                .iter()
                .map(|line| InlineState::shared((*line).into()))
                .collect();
        } else {
            for (state, line) in states[span.clone()].iter().zip(&lines[span.clone()]) {
                if let Ok(mut state) = state.lock() {
                    state.set_text((*line).into());
                }
            }
        }
        states[span].to_vec()
    }

    fn selected_text(&self) -> String {
        if let Some(text) = self.formula.selected_text() {
            return text;
        }
        let Ok(states) = self.line_states.lock() else {
            return String::new();
        };
        let (Some(first), Some(last)) = (
            states
                .iter()
                .position(|state| selected_inline_text(state).is_some()),
            states
                .iter()
                .rposition(|state| selected_inline_text(state).is_some()),
        ) else {
            return String::new();
        };
        // A selection is contiguous, so the lines between its ends are whole,
        // including lines that were not painted while it grew.
        (first..=last)
            .map(|ix| {
                if ix == first || ix == last {
                    selected_inline_text(&states[ix]).unwrap_or_default()
                } else {
                    states[ix]
                        .lock()
                        .map_or_else(|_| String::new(), |state| state.text.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn clear_selection(&self) {
        self.formula.clear();
        if let Ok(states) = self.line_states.lock() {
            states.iter().for_each(clear_inline_selection);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Table {
    pub(crate) children: Vec<TableRow>,
    pub(crate) column_aligns: Vec<ColumnumnAlign>,
}

impl Table {
    pub(crate) fn column_align(&self, index: usize) -> ColumnumnAlign {
        self.column_aligns.get(index).copied().unwrap_or_default()
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub(crate) enum ColumnumnAlign {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TableRow {
    pub(crate) children: Vec<TableCell>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TableCell {
    pub(crate) children: Paragraph,
}

fn selected_inline_text(state: &Arc<Mutex<InlineState>>) -> Option<String> {
    let state = state.lock().ok()?;
    let selection = state.selection.as_ref()?;
    state
        .text
        .get(selection.clone())
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn append_selection(text: &mut String, state: &Arc<Mutex<InlineState>>) {
    if let Some(selected) = selected_inline_text(state) {
        text.push_str(&selected);
    }
}

fn clear_inline_selection(state: &Arc<Mutex<InlineState>>) {
    if let Ok(mut state) = state.lock() {
        state.selection = None;
    }
}

fn select_inline(state: &Arc<Mutex<InlineState>>, from: Option<usize>, to: Option<usize>) {
    if let Ok(mut state) = state.lock() {
        let len = state.text.len();
        let from = from.unwrap_or(0).min(len);
        state.selection = Some(from..to.unwrap_or(len).clamp(from, len));
    }
}

fn rendered_code_text(code: &str) -> String {
    let mut text = code.replace("\r\n", "\n");
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(state: &Arc<Mutex<InlineState>>, range: Range<usize>) {
        state.lock().unwrap().selection = Some(range);
    }

    fn select_paragraph(paragraph: &Paragraph, range: Range<usize>) {
        let mut state = paragraph.state.lock().unwrap();
        state.set_text(paragraph.text().into());
        state.selection = Some(range);
    }

    #[test]
    fn extraction_includes_unpainted_blocks_between_selected_boundaries() {
        let document = super::super::parse(
            "intro text\n\n```text\nfirst code line\nsecond code line\n```\n\n| name | value |\n| --- | --- |\n| alpha | beta |",
        );
        let mut leaves = Vec::new();
        document.collect_text_leaves(&mut leaves);

        let BlockNode::Paragraph(paragraph) = leaves[0] else {
            panic!("expected paragraph");
        };
        select_paragraph(paragraph, 0..paragraph.text().len());
        assert_eq!(paragraph.selected_text(), "intro text");

        let BlockNode::Table(table) = leaves[2] else {
            panic!("expected table");
        };
        for row in &table.children {
            for cell in &row.children {
                select_paragraph(&cell.children, 0..cell.children.text().len());
            }
        }

        assert_eq!(
            document.selected_text(),
            "intro text\nfirst code line\nsecond code line\nname value\nalpha beta"
        );
    }

    #[test]
    fn extraction_preserves_partial_code_and_table_boundaries() {
        let document = super::super::parse(
            "intro text\n\n```text\nfirst code line\nsecond code line\n```\n\n| name | value |\n| --- | --- |\n| alpha | beta |",
        );
        let mut leaves = Vec::new();
        document.collect_text_leaves(&mut leaves);

        let BlockNode::CodeBlock(code) = leaves[1] else {
            panic!("expected code block");
        };
        let states = code.states_for_lines(&["first code line", "second code line"], 0..2);
        select(&states[0], 6..10);
        select(&states[1], 0..6);

        let BlockNode::Table(table) = leaves[2] else {
            panic!("expected table");
        };
        let first_cell = &table.children[0].children[0].children;
        let second_cell = &table.children[0].children[1].children;
        select_paragraph(first_cell, 1..4);
        select_paragraph(second_cell, 0..3);

        assert_eq!(document.selected_text(), "code\nsecond\name val");
    }
}
