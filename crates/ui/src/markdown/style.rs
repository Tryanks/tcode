//! Markdown presentation defaults, adapted from gpui-component's Apache-2.0
//! `text/style.rs` implementation.

use std::sync::Arc;

use gpui::{Pixels, Rems, StyleRefinement, px, rems};

/// Presentation settings for [`super::MarkdownView`].
#[derive(Clone)]
pub struct TextViewStyle {
    /// Gap between top-level paragraphs and blocks.
    pub paragraph_gap: Rems,
    /// Base body font size.
    pub base_font_size: Pixels,
    /// Base used by the heading-size scale.
    pub heading_base_font_size: Pixels,
    /// Optional heading size override for levels 1 through 6.
    pub heading_font_size: Option<Arc<dyn Fn(u8, Pixels) -> Pixels + Send + Sync + 'static>>,
    /// Inline-code font size.
    pub inline_code_font_size: Pixels,
    /// Inline-code corner radius.
    pub inline_code_radius: Pixels,
    /// Additional style for fenced and indented code blocks.
    pub code_block: StyleRefinement,
    /// Additional style for the table viewport.
    pub table: StyleRefinement,
    /// Additional style for each table cell.
    pub table_cell: StyleRefinement,
}

impl PartialEq for TextViewStyle {
    fn eq(&self, other: &Self) -> bool {
        self.paragraph_gap == other.paragraph_gap
            && self.base_font_size == other.base_font_size
            && self.heading_base_font_size == other.heading_base_font_size
            && self.inline_code_font_size == other.inline_code_font_size
            && self.inline_code_radius == other.inline_code_radius
    }
}

impl Default for TextViewStyle {
    fn default() -> Self {
        Self {
            paragraph_gap: rems(1.),
            base_font_size: px(15.),
            heading_base_font_size: px(15.),
            heading_font_size: None,
            inline_code_font_size: px(13.),
            inline_code_radius: px(4.),
            code_block: StyleRefinement::default(),
            table: StyleRefinement::default(),
            table_cell: StyleRefinement::default(),
        }
    }
}

impl TextViewStyle {
    pub fn paragraph_gap(mut self, gap: Rems) -> Self {
        self.paragraph_gap = gap;
        self
    }

    pub fn heading_font_size<F>(mut self, f: F) -> Self
    where
        F: Fn(u8, Pixels) -> Pixels + Send + Sync + 'static,
    {
        self.heading_font_size = Some(Arc::new(f));
        self
    }

    pub fn code_block(mut self, style: StyleRefinement) -> Self {
        self.code_block = style;
        self
    }

    pub fn table(mut self, style: StyleRefinement) -> Self {
        self.table = style;
        self
    }

    pub fn table_cell(mut self, style: StyleRefinement) -> Self {
        self.table_cell = style;
        self
    }
}
