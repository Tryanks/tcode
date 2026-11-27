//! The text of a visual: the runs a frame paints, numbered in the order
//! they are drawn, which is the order the page reads in. The conversation's
//! selection takes its ends and what it copies from them.
//!
//! What is drawn but is not the page's text takes no part: a list's marker,
//! a `::before`, the value of a form control, and the text inside an
//! `<svg>`, which is drawn as a picture.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, CursorStyle, Element, ElementId, GlobalElementId, HighlightStyle, Hitbox,
    HitboxBehavior, InspectorElementId, LayoutId, Pixels, SharedString, StyledText, TextLayout,
    Window,
};

use crate::markdown::nodes::TextPosition;
use crate::markdown::selection::{
    EntryText, Membership, PaintedRun, PaintedRuns, RunOrigin, paint_selected,
};

/// What comes between a run and the one before it in the same line of
/// inline boxes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Lead {
    #[default]
    Nothing,
    /// A `<br>`.
    Break,
}

struct Segment {
    text: SharedString,
    /// The table row and the cell of it the run is in.
    row: Option<u32>,
    cell: Option<u32>,
    /// The line of inline boxes the run is one of.
    flow: Option<u32>,
    lead: Lead,
}

/// The text a visual draws, by the positions its runs are painted at.
#[derive(Default)]
pub(crate) struct PageText {
    segments: Vec<Segment>,
}

fn position(segment: usize, offset: usize) -> TextPosition {
    TextPosition {
        path: vec![segment as u32],
        line: 0,
        offset,
    }
}

impl EntryText for PageText {
    /// Runs on their own lines, the cells of a table row separated by tabs,
    /// and the boxes of one line joined as the page spaces them.
    fn text_between(&self, start: &TextPosition, end: &TextPosition) -> String {
        let mut text = String::new();
        let mut before: Option<&Segment> = None;
        for (ix, segment) in self.segments.iter().enumerate() {
            let (first, last) = (position(ix, 0), position(ix, segment.text.len()));
            if *end <= first || *start >= last {
                continue;
            }
            let at = |position: &TextPosition, default: usize| {
                if position.path == first.path {
                    segment
                        .text
                        .floor_char_boundary(position.offset.min(segment.text.len()))
                } else {
                    default
                }
            };
            let (from, to) = (at(start, 0), at(end, segment.text.len()));
            let Some(piece) = segment.text.get(from..to).filter(|piece| !piece.is_empty()) else {
                continue;
            };
            if let Some(before) = before {
                let same = |a: Option<u32>, b: Option<u32>| a.is_some() && a == b;
                text.push_str(if same(before.flow, segment.flow) {
                    match segment.lead {
                        Lead::Nothing => "",
                        Lead::Break => "\n",
                    }
                } else if same(before.row, segment.row) && before.cell != segment.cell {
                    "\t"
                } else {
                    "\n"
                });
            }
            text.push_str(piece);
            before = Some(segment);
        }
        text
    }

    fn extent(&self) -> Option<(TextPosition, TextPosition)> {
        let last = self.segments.len().checked_sub(1)?;
        Some((
            position(0, 0),
            position(last, self.segments[last].text.len()),
        ))
    }

    fn segment_text(&self, path: &[u32], line: u32) -> Option<String> {
        let [segment] = path else {
            return None;
        };
        (line == 0)
            .then(|| self.segments.get(*segment as usize))
            .flatten()
            .map(|segment| segment.text.to_string())
    }
}

/// Where a visual's text is selected: the document it is an entry of, and
/// the runs it paints there.
#[derive(Clone)]
pub(super) struct Selecting {
    pub(super) membership: Membership,
    pub(super) runs: PaintedRuns,
}

/// Takes the runs of a frame as it is built, in the order they are drawn.
#[derive(Default)]
pub(super) struct TextSink {
    pub(super) text: PageText,
    pub(super) selecting: Option<Selecting>,
    row: Option<u32>,
    cell: Option<u32>,
    flow: Option<u32>,
    lead: Lead,
    /// Inside a link, whose pointer the text leaves as it is.
    link: bool,
    scopes: u32,
}

/// The part of a page a [`TextSink`] is in, to be given back to
/// [`TextSink::leave`].
pub(super) struct Scope {
    row: Option<u32>,
    cell: Option<u32>,
    flow: Option<u32>,
    link: bool,
}

impl TextSink {
    pub(super) fn selecting(selecting: Option<Selecting>) -> Self {
        Self {
            selecting,
            ..Self::default()
        }
    }

    fn scope(&mut self) -> (Scope, u32) {
        self.scopes += 1;
        (
            Scope {
                row: self.row,
                cell: self.cell,
                flow: self.flow,
                link: self.link,
            },
            self.scopes,
        )
    }

    /// A table row begins.
    pub(super) fn enter_row(&mut self) -> Scope {
        let (outer, id) = self.scope();
        (self.row, self.cell, self.flow) = (Some(id), None, None);
        outer
    }

    /// A cell of the row begins.
    pub(super) fn enter_cell(&mut self) -> Scope {
        let (outer, id) = self.scope();
        (self.cell, self.flow) = (Some(id), None);
        outer
    }

    /// A line of inline boxes begins, unless it is inside one already.
    pub(super) fn enter_flow(&mut self) -> Scope {
        let (outer, id) = self.scope();
        if self.flow.is_none() {
            self.flow = Some(id);
            self.lead = Lead::Nothing;
        }
        outer
    }

    pub(super) fn enter_link(&mut self) -> Scope {
        let (outer, _) = self.scope();
        self.link = true;
        outer
    }

    pub(super) fn leave(&mut self, outer: Scope) {
        (self.row, self.cell, self.flow, self.link) =
            (outer.row, outer.cell, outer.flow, outer.link);
    }

    /// The page breaks the line between the box before and the next.
    pub(super) fn line_break(&mut self) {
        self.lead = Lead::Break;
    }

    /// The next run of the page: `text`, styled apart from its box over
    /// `highlights`.
    pub(super) fn run(
        &mut self,
        text: String,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    ) -> Run {
        let text = SharedString::from(text);
        let segment = self.text.segments.len() as u32;
        self.text.segments.push(Segment {
            text: text.clone(),
            row: self.row,
            cell: self.cell,
            flow: self.flow,
            lead: std::mem::take(&mut self.lead),
        });
        let styled = StyledText::new(text.clone());
        let styled = if highlights.is_empty() {
            styled
        } else {
            styled.with_highlights(highlights)
        };
        Run {
            layout: styled.layout().clone(),
            styled,
            text,
            selecting: self
                .selecting
                .clone()
                .map(|selecting| (selecting, RunOrigin::new(Rc::from([segment])))),
            ibeam: !self.link,
            selected: None,
        }
    }
}

/// A run of a visual's text, which the selection its visual is in selects.
pub(super) struct Run {
    text: SharedString,
    styled: StyledText,
    layout: TextLayout,
    selecting: Option<(Selecting, RunOrigin)>,
    ibeam: bool,
    /// The part of the text selected, as of prepaint.
    selected: Option<Range<usize>>,
}

impl Element for Run {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.styled.request_layout(None, None, window, cx)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled
            .prepaint(None, None, bounds, &mut (), window, cx);
        let (selecting, origin) = self.selecting.as_ref()?;
        let run = PaintedRun {
            origin: origin.clone(),
            text: self.text.clone(),
            layout: self.layout.clone(),
            bounds,
            visible: window.content_mask().bounds,
        };
        self.selected = selecting
            .membership
            .selected_range(origin, self.text.len(), cx);
        if let Some(selected) = &self.selected {
            selecting.membership.register_edge(&run, selected, cx);
        }
        selecting.runs.push(run);
        self.ibeam
            .then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(hitbox) = hitbox {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }
        self.styled
            .paint(None, None, bounds, &mut (), &mut (), window, cx);
        if let Some(selected) = &self.selected {
            paint_selected(selected, &self.layout, bounds, window, cx);
        }
    }
}
