//! Markdown render IR.
//!
//! Selection storage is adapted from gpui-component's Apache-2.0 text node
//! implementation; parsing and the node shapes themselves are tcode-owned.

use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

use gpui::{DefiniteLength, Hsla, SharedString, SharedUri};

use super::inline::InlineState;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BlockNode {
    Root {
        children: Vec<BlockNode>,
        span: Option<Span>,
    },
    Paragraph(Paragraph),
    Heading {
        level: u8,
        children: Paragraph,
        span: Option<Span>,
    },
    Blockquote {
        children: Vec<BlockNode>,
        span: Option<Span>,
    },
    List {
        children: Vec<BlockNode>,
        ordered: bool,
        span: Option<Span>,
    },
    ListItem {
        children: Vec<BlockNode>,
        spread: bool,
        checked: Option<bool>,
        span: Option<Span>,
    },
    CodeBlock(CodeBlock),
    Table(Table),
    HorizontalRule {
        span: Option<Span>,
    },
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
            Self::CodeBlock(code) => code.code.to_string(),
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
            Self::HorizontalRule { .. } | Self::Unknown => String::new(),
        }
    }

    pub(super) fn selected_text(&self) -> String {
        match self {
            Self::Root { children, .. }
            | Self::Blockquote { children, .. }
            | Self::List { children, .. }
            | Self::ListItem { children, .. } => join_selected(children.iter()),
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
            Self::HorizontalRule { .. } | Self::Unknown => String::new(),
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
            Self::HorizontalRule { .. } | Self::Unknown => {}
        }
    }
}

fn join_selected<'a>(nodes: impl Iterator<Item = &'a BlockNode>) -> String {
    nodes
        .map(BlockNode::selected_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct LinkMark {
    pub(crate) url: SharedString,
    pub(crate) identifier: Option<SharedString>,
    pub(crate) title: Option<SharedString>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct TextMark {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) strikethrough: bool,
    pub(crate) underline: bool,
    pub(crate) code: bool,
    pub(crate) highlight: Option<Hsla>,
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
        self.underline |= other.underline;
        self.code |= other.code;
        if other.highlight.is_some() {
            self.highlight = other.highlight;
        }
        if other.link.is_some() {
            self.link = other.link;
        }
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ImageNode {
    pub(crate) url: SharedUri,
    pub(crate) link: Option<LinkMark>,
    pub(crate) title: Option<SharedString>,
    pub(crate) alt: Option<SharedString>,
    pub(crate) width: Option<DefiniteLength>,
    pub(crate) height: Option<DefiniteLength>,
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
            && self.width == other.width
            && self.height == other.height
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct InlineNode {
    pub(crate) text: SharedString,
    pub(crate) image: Option<ImageNode>,
    pub(crate) marks: Vec<(Range<usize>, TextMark)>,
    pub(super) state: Arc<Mutex<InlineState>>,
}

impl PartialEq for InlineNode {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.image == other.image && self.marks == other.marks
    }
}

impl InlineNode {
    pub(crate) fn new(text: impl Into<SharedString>) -> Self {
        let text = text.into();
        Self {
            state: InlineState::shared(text.clone()),
            text,
            image: None,
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
    pub(crate) span: Option<Span>,
    pub(crate) children: Vec<InlineNode>,
    pub(super) state: Arc<Mutex<InlineState>>,
}

impl PartialEq for Paragraph {
    fn eq(&self, other: &Self) -> bool {
        self.span == other.span && self.children == other.children
    }
}

impl Paragraph {
    pub(crate) fn text(&self) -> String {
        self.children
            .iter()
            .map(|node| node.text.as_ref())
            .collect()
    }

    pub(super) fn selected_text(&self) -> String {
        let mut text = String::new();
        for child in &self.children {
            append_selection(&mut text, &child.state);
        }
        append_selection(&mut text, &self.state);
        text
    }

    pub(super) fn clear_selection(&self) {
        for child in &self.children {
            clear_inline_selection(&child.state);
        }
        clear_inline_selection(&self.state);
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CodeBlock {
    pub(crate) code: SharedString,
    pub(crate) lang: Option<SharedString>,
    pub(crate) span: Option<Span>,
    pub(super) line_states: Arc<Mutex<Vec<Arc<Mutex<InlineState>>>>>,
}

impl PartialEq for CodeBlock {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.lang == other.lang && self.span == other.span
    }
}

impl CodeBlock {
    pub(super) fn states_for_lines(&self, lines: &[&str]) -> Vec<Arc<Mutex<InlineState>>> {
        let Ok(mut states) = self.line_states.lock() else {
            return lines
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
            for (state, line) in states.iter().zip(lines) {
                if let Ok(mut state) = state.lock() {
                    state.set_text((*line).into());
                }
            }
        }
        states.clone()
    }

    fn selected_text(&self) -> String {
        let Ok(states) = self.line_states.lock() else {
            return String::new();
        };
        let selected = states
            .iter()
            .enumerate()
            .filter_map(|(ix, state)| selected_inline_text(state).map(|text| (ix, text)))
            .collect::<Vec<_>>();
        let (Some((first, _)), Some((last, _))) = (selected.first(), selected.last()) else {
            return String::new();
        };
        let (first, last) = (*first, *last);
        let mut lines = vec![String::new(); last - first + 1];
        for (ix, text) in selected {
            lines[ix - first] = text;
        }
        lines.join("\n")
    }

    fn clear_selection(&self) {
        if let Ok(states) = self.line_states.lock() {
            states.iter().for_each(clear_inline_selection);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Table {
    pub(crate) children: Vec<TableRow>,
    pub(crate) column_aligns: Vec<ColumnumnAlign>,
    pub(crate) span: Option<Span>,
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
