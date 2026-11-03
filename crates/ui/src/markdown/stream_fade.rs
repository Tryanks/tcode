//! Streamed text lights up in reading order: the rendered text an append
//! adds is cut into words (CJK characters) in document order, across every
//! paragraph, list item, cell and code line it touches, and each word starts
//! fading in no earlier than the one before it, on one timeline that carries
//! over from one append to the next. An append too large to have been
//! written while the reader watched fades in as one document instead.
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

use super::nodes::BlockNode;

/// The step between consecutive words of one append.
const STEP: Duration = Duration::from_millis(28);

/// How far ahead of now the timeline may run. Past it the step shrinks, down
/// to [`MIN_STEP`], so a burst sweeps in quickly instead of queueing
/// invisible text, and still in order.
const BUDGET: Duration = Duration::from_millis(320);
const MIN_STEP: Duration = Duration::from_millis(4);

/// A word reaches most of its opacity quickly and the rest slowly: legible
/// at once, while a batch of words arriving together never flashes in.
const WORD_QUICK: Duration = Duration::from_millis(120);
const WORD_SETTLE: Duration = Duration::from_millis(560);
const WORD_QUICK_SHARE: f32 = 0.7;

/// An append adding more rendered characters than this was not written
/// while the reader watched (a provider delivering whole blocks, a stall
/// catching up): it fades in as one document, together with the height
/// glide of the row that holds it, rather than sweeping in line by line.
const ONE_SHOT_CHARS: usize = 512;
const DOCUMENT: Duration = Duration::from_millis(320);

/// A text leaf: the render path of a paragraph, heading or table cell, or of
/// a code block with the line in it -- the place selection runs name.
type LeafKey = (Vec<u32>, u32);

/// Byte ranges of a run with the [`HighlightStyle::fade_out`] each paints
/// with this frame: `1.0` transparent, `0.0` at full colour.
pub(super) type Fades = Vec<(Range<usize>, f32)>;

#[derive(Clone, Copy, PartialEq)]
enum Curve {
    Word,
    Document,
}

impl Curve {
    fn duration(self) -> Duration {
        match self {
            Curve::Word => WORD_SETTLE,
            Curve::Document => DOCUMENT,
        }
    }

    /// The opacity `elapsed` into the fade, 0 to 1.
    fn opacity(self, elapsed: Duration) -> f32 {
        let progress = |over: Duration| (elapsed.as_secs_f32() / over.as_secs_f32()).min(1.);
        match self {
            Curve::Word => {
                let ease = gpui_base::animation::ease_out_cubic;
                WORD_QUICK_SHARE * ease(progress(WORD_QUICK))
                    + (1. - WORD_QUICK_SHARE) * ease(progress(WORD_SETTLE))
            }
            Curve::Document => gpui_base::animation::ease_in_out_cubic(progress(DOCUMENT)),
        }
    }
}

struct Segment {
    range: Range<usize>,
    start: Instant,
    curve: Curve,
}

/// Which rendered text arrived recently, so its fade can be sampled.
#[derive(Default)]
pub(super) struct StreamFade {
    leaves: Vec<(LeafKey, Vec<Segment>)>,
    /// When the word after the last one scheduled may start.
    next_start: Option<Instant>,
}

impl StreamFade {
    /// Forget every fade: the text is shown as it is.
    pub(super) fn clear(&mut self) {
        self.leaves.clear();
        self.next_start = None;
    }

    /// Schedule what root blocks `new` render that `old` did not, both
    /// starting at root block `first_block`. Whether the append was one
    /// document, which fades in whole.
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
        let previous: HashMap<LeafKey, String> = leaf_texts(old, first_block).into_iter().collect();
        let current = leaf_texts(new, first_block);
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
            if (key.0[0] as usize) < first_block {
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
        let document = added > ONE_SHOT_CHARS;
        let units: Vec<(usize, Range<usize>)> = current
            .iter()
            .zip(&shown)
            .enumerate()
            .filter(|(_, ((_, text), shown))| **shown < text.len())
            .flat_map(|(ix, ((_, text), shown))| {
                let units = if document {
                    std::iter::once(*shown..text.len()).collect()
                } else {
                    fade_units(text, *shown, text.len())
                };
                units.into_iter().map(move |range| (ix, range))
            })
            .collect();

        let count = units.len() as u32;
        let budget_end = now + BUDGET;
        let first = self
            .next_start
            .map_or(now, |at| at.max(now))
            .min(budget_end);
        let step = if document || count < 2 {
            Duration::ZERO
        } else if first + STEP * (count - 1) > budget_end {
            (budget_end.saturating_duration_since(first) / (count - 1)).max(MIN_STEP)
        } else {
            STEP
        };
        let curve = if document {
            Curve::Document
        } else {
            Curve::Word
        };
        for (position, (ix, range)) in units.into_iter().enumerate() {
            let key = &current[ix].0;
            let segment = Segment {
                range,
                start: first + step * position as u32,
                curve,
            };
            match self.leaves.iter_mut().find(|(leaf, _)| leaf == key) {
                Some((_, segments)) => segments.push(segment),
                None => self.leaves.push((key.clone(), vec![segment])),
            }
        }
        self.next_start = Some(first + step * count);
        document
    }

    /// Sample every unfinished fade at `now`, dropping the finished ones.
    /// `None` when nothing fades, so no frame needs to follow.
    pub(super) fn frame(&mut self, now: Instant, reduce_motion: bool) -> Option<Rc<FadeFrame>> {
        if reduce_motion {
            self.clear();
        }
        if self.leaves.is_empty() {
            self.next_start = None;
            return None;
        }
        let mut leaves = Vec::with_capacity(self.leaves.len());
        self.leaves.retain_mut(|(key, segments)| {
            let mut fades = Vec::with_capacity(segments.len());
            segments.retain(|segment| {
                let Some(elapsed) = now.checked_duration_since(segment.start) else {
                    fades.push((segment.range.clone(), 1.));
                    return true;
                };
                if elapsed >= segment.curve.duration() {
                    return false;
                }
                let opacity = segment.curve.opacity(elapsed);
                fades.push((segment.range.clone(), (1. - opacity).clamp(0., 1.)));
                true
            });
            if fades.is_empty() {
                return false;
            }
            fades.sort_by_key(|(range, _)| range.start);
            leaves.push((key.clone(), fades));
            true
        });
        (!leaves.is_empty()).then(|| Rc::new(FadeFrame { leaves }))
    }
}

/// One frame's fades, sampled once per render of the document.
pub(super) struct FadeFrame {
    leaves: Vec<(LeafKey, Fades)>,
}

impl FadeFrame {
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
/// `first_block`, in reading order.
fn leaf_texts(blocks: &[BlockNode], first_block: usize) -> Vec<(LeafKey, String)> {
    let mut texts = Vec::new();
    for (offset, block) in blocks.iter().enumerate() {
        block.for_each_segment_at(first_block + offset, &mut |path, line, text, _| {
            texts.push(((path.to_vec(), line), text.to_string()));
        });
    }
    texts
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
