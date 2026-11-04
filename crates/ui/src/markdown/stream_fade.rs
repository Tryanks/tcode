//! Streamed text lights up in reading order: the rendered text an append
//! adds is cut into words (CJK characters) in document order, across every
//! paragraph, list item, cell and code line it touches, and the words start
//! fading in one after another at the pace of what is still waiting: word by
//! word for a few, faster the more wait. The word the reveal has reached is
//! its frontier, which the chat timeline's tail follows, so the reveal and
//! the scroll are one motion.
//!
//! gpui-base fades streamed text only inside its own `TextView`
//! (`TextViewMotion::stream_fade`): its tracker is private to that view and
//! bound to the document it parses, and it schedules each update from now,
//! so updates and the leaves within one update can overtake each other.
//! This is built here on Tcode's render IR, painted through
//! `HighlightStyle::fade_out` with decoration colours faded alongside,
//! until gpui-base exposes an ordered tracker for a document it does not own.

use std::{collections::HashMap, ops::Range, rc::Rc, time::Duration};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::HighlightStyle;

use super::nodes::{BlockNode, SegmentKind};

/// The step between consecutive words, while few wait.
const STEP: Duration = Duration::from_millis(28);

/// Past the few that one [`STEP`] each suits, what waits drains by this time
/// constant: a sustained stream runs this far behind what arrived, and a
/// block slows into the word-by-word pace as it ends.
const DRAIN: Duration = Duration::from_millis(160);

/// The most words that may wait. Those before them start at once: no reader
/// follows them, and this bounds any block's reveal at about a second.
const MAX_BACKLOG: f32 = 1000.;

/// A word reaches most of its opacity quickly and the rest slowly: legible
/// at once, while a batch of words arriving together never flashes in.
const WORD_QUICK: Duration = Duration::from_millis(120);
const WORD_SETTLE: Duration = Duration::from_millis(560);
const WORD_QUICK_SHARE: f32 = 0.7;

/// An append adding more rendered characters than this was not written
/// while the reader watched: a provider delivering whole blocks, a stall
/// catching up.
const ONE_SHOT_CHARS: usize = 512;

/// A text leaf: the render path of a paragraph, heading or table cell, or of
/// a code block with the line in it -- the place selection runs name.
type LeafKey = (Vec<u32>, u32);

/// A byte offset in a leaf.
type LeafPlace = (LeafKey, usize);

/// Byte ranges of a run with the [`HighlightStyle::fade_out`] each paints
/// with this frame: `1.0` transparent, `0.0` at full colour.
pub(super) type Fades = Vec<(Range<usize>, f32)>;

/// How far into its fade a word that started `elapsed` ago is, 0 to 1.
fn opacity(elapsed: Duration) -> f32 {
    let progress = |over: Duration| (elapsed.as_secs_f32() / over.as_secs_f32()).min(1.);
    let ease = gpui_base::animation::ease_out_cubic;
    WORD_QUICK_SHARE * ease(progress(WORD_QUICK))
        + (1. - WORD_QUICK_SHARE) * ease(progress(WORD_SETTLE))
}

/// The words at which draining by [`DRAIN`] has slowed to one [`STEP`] a word.
const DRAIN_FLOOR: f32 = DRAIN.as_secs_f32() / STEP.as_secs_f32();

/// How long `backlog` words take to start.
fn time_to_drain(backlog: f32) -> f32 {
    if backlog <= DRAIN_FLOOR {
        backlog * STEP.as_secs_f32()
    } else {
        DRAIN.as_secs_f32() * (1. + (backlog / DRAIN_FLOOR).ln())
    }
}

/// The words left with `time` to go: [`time_to_drain`]'s inverse.
fn backlog_after(time: f32) -> f32 {
    if time <= DRAIN.as_secs_f32() {
        time.max(0.) / STEP.as_secs_f32()
    } else {
        DRAIN_FLOOR * (time / DRAIN.as_secs_f32() - 1.).exp()
    }
}

struct Segment {
    range: Range<usize>,
    /// Its place in reading order among every word ever scheduled.
    order: u64,
    start: Option<Instant>,
}

/// Which rendered text arrived recently, so its fade can be sampled.
#[derive(Default)]
pub(super) struct StreamFade {
    leaves: Vec<(LeafKey, Vec<Segment>)>,
    /// The next word's place in reading order.
    scheduled: u64,
    /// The words yet to start, as of `paced_at`. A fraction is the part of
    /// a step already waited.
    backlog: f32,
    paced_at: Option<Instant>,
}

impl StreamFade {
    /// Forget every fade: the text is shown as it is.
    pub(super) fn clear(&mut self) {
        self.leaves.clear();
        self.backlog = 0.;
        self.paced_at = None;
    }

    /// When the last word waiting starts, while any waits.
    pub(super) fn drains_at(&self) -> Option<Instant> {
        let paced_at = self.paced_at.filter(|_| self.backlog > 0.)?;
        Some(paced_at + Duration::from_secs_f32(time_to_drain(self.backlog)))
    }

    /// Bring the backlog up to `now`.
    fn pace(&mut self, now: Instant) {
        if let Some(paced_at) = self.paced_at {
            let elapsed = now.saturating_duration_since(paced_at).as_secs_f32();
            self.backlog = backlog_after(time_to_drain(self.backlog) - elapsed);
        }
        self.paced_at = Some(now);
    }

    /// Schedule what root blocks `new` render that `old` did not, both
    /// starting at root block `first_block`. Whether the append was too
    /// large to have been written while the reader watched.
    ///
    /// Per leaf, the text up to the length it had before is already shown,
    /// even where a re-parse rewrote it (`**bo` becoming bold `bo`): only
    /// text past that length fades, so markup completing never fades glyphs
    /// the reader has seen.
    pub(super) fn record(
        &mut self,
        old: &[BlockNode],
        new: &[BlockNode],
        first_block: usize,
        now: Instant,
    ) -> bool {
        // A code line both hold as it was adds nothing and keeps its fades:
        // a long block streaming in is compared, not copied, up to its end.
        let same = same_code_lines(old, new, first_block);
        let previous: HashMap<LeafKey, String> =
            leaf_texts(old, first_block, &same).into_iter().collect();
        let current = leaf_texts(new, first_block, &same);
        let shown: Vec<usize> = current
            .iter()
            .map(|(key, text)| {
                previous.get(key).map_or(0, |old| {
                    if text.starts_with(old.as_str()) {
                        old.len()
                    } else {
                        floor_boundary(text, old.len())
                    }
                })
            })
            .collect();
        let texts: HashMap<&LeafKey, &str> = current
            .iter()
            .map(|(key, text)| (key, text.as_str()))
            .collect();
        // A leaf that is gone describes nothing on screen; one that changed
        // keeps its fades over the text it still has.
        self.leaves.retain_mut(|(key, segments)| {
            if (key.0[0] as usize) < first_block || is_same(&same, key) {
                return true;
            }
            let Some(text) = texts.get(key) else {
                return false;
            };
            segments.retain_mut(|segment| {
                segment.range.start = floor_boundary(text, segment.range.start);
                segment.range.end = floor_boundary(text, segment.range.end);
                segment.range.start < segment.range.end
            });
            !segments.is_empty()
        });

        let added: usize = current
            .iter()
            .zip(&shown)
            .map(|((_, text), shown)| text[*shown..].chars().count())
            .sum();
        if added == 0 {
            return false;
        }
        let units: Vec<(usize, Range<usize>)> = current
            .iter()
            .zip(&shown)
            .enumerate()
            .filter(|(_, ((_, text), shown))| **shown < text.len())
            .flat_map(|(ix, ((_, text), shown))| {
                fade_units(text, *shown, text.len())
                    .into_iter()
                    .map(move |range| (ix, range))
            })
            .collect();

        self.pace(now);
        self.backlog = (self.backlog + units.len() as f32).min(MAX_BACKLOG);
        for (ix, range) in units {
            let key = &current[ix].0;
            let segment = Segment {
                range,
                order: self.scheduled,
                start: None,
            };
            self.scheduled += 1;
            match self.leaves.iter_mut().find(|(leaf, _)| leaf == key) {
                Some((_, segments)) => segments.push(segment),
                None => self.leaves.push((key.clone(), vec![segment])),
            }
        }
        added > ONE_SHOT_CHARS
    }

    /// Sample every unfinished fade at `now`, starting the words the pace
    /// has reached and dropping the finished ones. `None` when nothing
    /// fades, so no frame needs to follow.
    pub(super) fn frame(&mut self, now: Instant, reduce_motion: bool) -> Option<Rc<FadeFrame>> {
        if reduce_motion || self.leaves.is_empty() {
            self.clear();
            return None;
        }
        self.pace(now);
        let (scheduled, backlog) = (self.scheduled, self.backlog);
        let mut leaves = Vec::with_capacity(self.leaves.len());
        let mut reached: Option<(u64, LeafPlace)> = None;
        let mut waiting: Option<(u64, LeafPlace)> = None;
        self.leaves.retain_mut(|(key, segments)| {
            let mut fades = Vec::with_capacity(segments.len());
            let mut leaf_reached = None;
            let mut leaf_waiting = None;
            segments.retain_mut(|segment| {
                // A word starts once no more wait than itself and those after it.
                if segment.start.is_none() && (scheduled - segment.order) as f32 >= backlog {
                    segment.start = Some(now);
                }
                let place = (segment.order, segment.range.start);
                let Some(start) = segment.start else {
                    leaf_waiting = leaf_waiting.min(Some(place)).or(Some(place));
                    fades.push((segment.range.clone(), 1.));
                    return true;
                };
                leaf_reached = leaf_reached.max(Some(place));
                let elapsed = now.saturating_duration_since(start);
                if elapsed >= WORD_SETTLE {
                    return false;
                }
                fades.push((segment.range.clone(), (1. - opacity(elapsed)).clamp(0., 1.)));
                true
            });
            if let Some((order, offset)) = leaf_reached
                && reached.as_ref().is_none_or(|(last, _)| order > *last)
            {
                reached = Some((order, (key.clone(), offset)));
            }
            if let Some((order, offset)) = leaf_waiting
                && waiting.as_ref().is_none_or(|(first, _)| order < *first)
            {
                waiting = Some((order, (key.clone(), offset)));
            }
            if fades.is_empty() {
                return false;
            }
            fades.sort_by_key(|(range, _)| range.start);
            leaves.push((key.clone(), fades));
            true
        });
        let frontier = waiting.map(|(_, first)| reached.map_or(first, |(_, last)| last));
        (!leaves.is_empty()).then(|| Rc::new(FadeFrame { leaves, frontier }))
    }
}

/// One frame's fades, sampled once per render of the document.
pub(super) struct FadeFrame {
    leaves: Vec<(LeafKey, Fades)>,
    /// While words wait, the place of the last one that started.
    frontier: Option<LeafPlace>,
}

impl FadeFrame {
    /// While words wait, the render path, line and byte offset of the last
    /// one that started.
    pub(super) fn frontier(&self) -> Option<(&[u32], u32, usize)> {
        let ((path, line), offset) = self.frontier.as_ref()?;
        Some((path, *line, *offset))
    }

    /// The fades of the run of `len` bytes at `base` in leaf `path`, `line`,
    /// in the run's own bytes.
    pub(super) fn run(&self, path: &[u32], line: u32, base: usize, len: usize) -> Fades {
        let Some((_, fades)) = self
            .leaves
            .iter()
            .find(|((leaf, leaf_line), _)| *leaf_line == line && leaf.as_slice() == path)
        else {
            return Vec::new();
        };
        let end = base + len;
        fades
            .iter()
            .filter(|(range, _)| range.start < end && range.end > base)
            .map(|(range, fade)| {
                (
                    range.start.max(base) - base..range.end.min(end) - base,
                    *fade,
                )
            })
            .collect()
    }
}

/// `highlights` with `fades` layered over them. GPUI fades only the glyph
/// colour, so an explicit background or decoration colour fades here with
/// its text, or an inline code chip or a strikethrough would show first.
pub(super) fn fade_highlights(
    highlights: &[(Range<usize>, HighlightStyle)],
    fades: &[(Range<usize>, f32)],
) -> Vec<(Range<usize>, HighlightStyle)> {
    let fades = fades.iter().map(|(range, fade_out)| {
        (
            range.clone(),
            HighlightStyle {
                fade_out: Some(*fade_out),
                ..Default::default()
            },
        )
    });
    let mut combined: Vec<_> =
        gpui::combine_highlights(highlights.iter().cloned(), fades).collect();
    for (_, style) in &mut combined {
        let Some(fade_out) = style.fade_out else {
            continue;
        };
        for color in [
            style.background_color.as_mut(),
            style.underline.as_mut().and_then(|u| u.color.as_mut()),
            style.strikethrough.as_mut().and_then(|s| s.color.as_mut()),
        ]
        .into_iter()
        .flatten()
        {
            color.fade_out(fade_out);
        }
    }
    combined
}

/// The opacity of what surrounds a run of `len` bytes faded by `fades` --
/// an inline code chip, a list marker: full as soon as any of its text
/// shows. Fades are in order and do not overlap.
pub(super) fn surround_opacity(fades: &[(Range<usize>, f32)], len: usize) -> f32 {
    let mut covered = 0;
    let mut fade_out = 1f32;
    for (range, fade) in fades {
        if range.start > covered {
            return 1.;
        }
        covered = range.end;
        fade_out = fade_out.min(*fade);
    }
    if fades.is_empty() || covered < len {
        return 1.;
    }
    1. - fade_out
}

/// The rendered text of every leaf of `blocks`, root blocks from
/// `first_block`, in reading order, but for the code lines in `same`.
fn leaf_texts(
    blocks: &[BlockNode],
    first_block: usize,
    same: &[(Vec<u32>, u32)],
) -> Vec<(LeafKey, String)> {
    let mut texts = Vec::new();
    for (offset, block) in blocks.iter().enumerate() {
        block.for_each_segment_at(first_block + offset, &mut |path, line, text, kind| {
            if kind == SegmentKind::Code && is_same_line(same, path, line) {
                return;
            }
            texts.push(((path.to_vec(), line), text.to_string()));
        });
    }
    texts
}

/// For each code block `old` and `new` both hold at one render path, root
/// blocks from `first_block`, its path and how many of its first lines they
/// hold alike.
fn same_code_lines(
    old: &[BlockNode],
    new: &[BlockNode],
    first_block: usize,
) -> Vec<(Vec<u32>, u32)> {
    fn children(node: &BlockNode) -> Option<&[BlockNode]> {
        match node {
            BlockNode::Root { children }
            | BlockNode::Blockquote { children }
            | BlockNode::List { children, .. }
            | BlockNode::ListItem { children, .. } => Some(children),
            _ => None,
        }
    }
    fn walk(
        old: &BlockNode,
        new: &BlockNode,
        path: &mut Vec<u32>,
        same: &mut Vec<(Vec<u32>, u32)>,
    ) {
        match (old, new) {
            // A formula's leaf is its glyphs, not its lines.
            (BlockNode::CodeBlock(old), BlockNode::CodeBlock(new))
                if old.formula.text().is_none() && new.formula.text().is_none() =>
            {
                let (old, new) = (old.prepared(), new.prepared());
                let lines = (0..old.lines.len().min(new.lines.len()))
                    .take_while(|&ix| old.line(ix) == new.line(ix))
                    .count();
                if lines > 0 {
                    same.push((path.clone(), lines as u32));
                }
            }
            _ => {
                if let (Some(old), Some(new)) = (children(old), children(new)) {
                    for (ix, (old, new)) in old.iter().zip(new).enumerate() {
                        path.push(ix as u32);
                        walk(old, new, path, same);
                        path.pop();
                    }
                }
            }
        }
    }
    let mut same = Vec::new();
    for (offset, (old, new)) in old.iter().zip(new).enumerate() {
        walk(
            old,
            new,
            &mut vec![(first_block + offset) as u32],
            &mut same,
        );
    }
    same
}

fn is_same_line(same: &[(Vec<u32>, u32)], path: &[u32], line: u32) -> bool {
    same.iter()
        .any(|(at, lines)| line < *lines && at.as_slice() == path)
}

fn is_same(same: &[(Vec<u32>, u32)], (path, line): &LeafKey) -> bool {
    is_same_line(same, path, *line)
}

/// `ix`, or the char boundary of `text` before it.
fn floor_boundary(text: &str, ix: usize) -> usize {
    let mut ix = ix.min(text.len());
    while !text.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

/// Split `start..end` of `text` into the units that fade one after another:
/// a word with the whitespace after it, or one CJK character, which has no
/// spaces to reveal it by.
fn fade_units(text: &str, start: usize, end: usize) -> Vec<Range<usize>> {
    let mut units = Vec::new();
    let mut unit_start = start;
    let mut unit_has_glyph = false;
    let mut previous = text[..start].chars().next_back();
    for (ix, c) in text[start..end].char_indices() {
        let position = start + ix;
        let starts_unit = unit_has_glyph
            && !c.is_whitespace()
            && (is_cjk(c) || previous.is_some_and(|p| p.is_whitespace() || is_cjk(p)));
        if starts_unit {
            units.push(unit_start..position);
            unit_start = position;
            unit_has_glyph = false;
        }
        unit_has_glyph |= !c.is_whitespace();
        previous = Some(c);
    }
    if unit_start < end {
        units.push(unit_start..end);
    }
    units
}

fn is_cjk(c: char) -> bool {
    matches!(
        u32::from(c),
        0x3040..=0x30FF // Hiragana, Katakana
            | 0x3400..=0x4DBF // CJK Unified Ideographs Extension A
            | 0x4E00..=0x9FFF // CJK Unified Ideographs
            | 0xAC00..=0xD7AF // Hangul syllables
            | 0xF900..=0xFAFF // CJK Compatibility Ideographs
            | 0x20000..=0x2FA1F // CJK Unified Ideographs Extensions B and later
    )
}
