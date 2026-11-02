//! Streamed text fades in: the rendered characters an append adds start
//! transparent and reach full colour over [`FADE`], word by word (character
//! by character for CJK) [`STAGGER`] apart, or as one chunk when the append
//! is too long to light up within one fade.
//!
//! gpui-base fades streamed text only inside its own `TextView`
//! (`TextViewMotion::stream_fade`): its tracker is private to that view and
//! bound to the document it parses. This ports its rule onto Tcode's render
//! IR -- per text leaf, the rendered text an append shares with the previous
//! parse stays as it is and the rest fades; `HighlightStyle::fade_out` over
//! the leaf's runs, with decoration colours faded alongside -- until
//! gpui-base exposes a tracker for a document it does not own.

use std::{collections::HashMap, ops::Range, rc::Rc, time::Duration};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::HighlightStyle;
use gpui_base::motion::{Easing, Stagger, StaggerOrigin, Timing};

use super::nodes::BlockNode;

/// The fade gpui-component's `TextView::stream_fade(true)` plays, the value
/// gpui-kit recommends; gpui-base's own default is no fade.
pub(crate) const FADE: Duration = Duration::from_millis(280);
const STAGGER: Duration = Duration::from_millis(10);

/// An append that adds more rendered text than this appears at once.
///
/// A provider streams a few tokens per delta, and the chat applies whatever
/// arrived since its last frame in one append: tens of bytes while text is
/// being written, a few hundred after a stall. More than that was not
/// written while the reader watched -- a reconnect catching up, a provider
/// that delivers whole paragraphs -- and fading it would flash a screenful
/// in at once. gpui-base only drops the stagger for such an update, which
/// still fades it as one chunk.
const BURST_BYTES: usize = 512;

/// A text leaf: the render path of a paragraph, heading or table cell, or of
/// a code block with the line in it -- the place selection runs name.
type LeafKey = (Vec<u32>, u32);

/// Byte ranges of a run with the [`HighlightStyle::fade_out`] each paints
/// with this frame: `1.0` transparent, `0.0` at full colour.
pub(super) type Fades = Vec<(Range<usize>, f32)>;

struct Segment {
    range: Range<usize>,
    started_at: Instant,
}

/// Which rendered text arrived recently, so its fade can be sampled.
#[derive(Default)]
pub(super) struct StreamFade {
    leaves: Vec<(LeafKey, Vec<Segment>)>,
}

impl StreamFade {
    /// Forget every fade: the text is shown as it is.
    pub(super) fn clear(&mut self) {
        self.leaves.clear();
    }

    /// Record what root blocks `new` render that `old` did not, both
    /// starting at root block `first_block`, as fades starting at `now`.
    /// Whether the append was a burst, which appears at once.
    pub(super) fn record(
        &mut self,
        old: &[BlockNode],
        new: &[BlockNode],
        first_block: usize,
        now: Instant,
    ) -> bool {
        let previous = leaf_texts(old, first_block);
        let current = leaf_texts(new, first_block);
        let prefixes: HashMap<&LeafKey, usize> = current
            .iter()
            .map(|(key, text)| {
                let prefix = previous
                    .get(key)
                    .map_or(0, |old| common_prefix_len(text, old));
                (key, prefix)
            })
            .collect();
        // A leaf that is gone, or diverged, is painted anew from the point
        // where it diverged; fades past that point describe nothing on screen.
        self.leaves.retain_mut(|(key, segments)| {
            if (key.0[0] as usize) < first_block {
                return true;
            }
            let Some(&prefix) = prefixes.get(key) else {
                return false;
            };
            segments.retain_mut(|segment| {
                segment.range.end = segment.range.end.min(prefix);
                segment.range.start < segment.range.end
            });
            !segments.is_empty()
        });

        let added: Vec<_> = current
            .iter()
            .map(|(key, text)| (key, text, prefixes[key]))
            .filter(|(_, text, prefix)| *prefix < text.len())
            .collect();
        let added_bytes: usize = added
            .iter()
            .map(|(_, text, prefix)| text.len() - prefix)
            .sum();
        if added_bytes > BURST_BYTES {
            return true;
        }
        for (key, text, prefix) in added {
            let units = fade_units(text, prefix, text.len());
            let stagger = Stagger::new(stagger_step(units.len()), StaggerOrigin::First);
            let count = units.len();
            let segments = units.into_iter().enumerate().map(|(ix, range)| Segment {
                range,
                started_at: now + stagger.delay(ix, count),
            });
            match self.leaves.iter_mut().find(|(leaf, _)| leaf == key) {
                Some((_, existing)) => existing.extend(segments),
                None => self.leaves.push((key.clone(), segments.collect())),
            }
        }
        false
    }

    /// Sample every unfinished fade at `now`, dropping the finished ones.
    /// `None` when nothing fades, so no frame needs to follow.
    pub(super) fn frame(&mut self, now: Instant, reduce_motion: bool) -> Option<Rc<FadeFrame>> {
        if reduce_motion {
            self.leaves.clear();
        }
        if self.leaves.is_empty() {
            return None;
        }
        let timing = Timing::new(FADE).ease(Easing::EaseOut);
        let mut leaves = Vec::with_capacity(self.leaves.len());
        self.leaves.retain_mut(|(key, segments)| {
            let mut fades = Vec::with_capacity(segments.len());
            segments.retain(|segment| {
                let sample = timing.sample(now.saturating_duration_since(segment.started_at));
                if sample.finished {
                    return false;
                }
                fades.push((
                    segment.range.clone(),
                    (1. - sample.directed_progress).clamp(0., 1.),
                ));
                true
            });
            if fades.is_empty() {
                return false;
            }
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

/// The rendered text of every leaf of `blocks`, root blocks from `first_block`.
fn leaf_texts(blocks: &[BlockNode], first_block: usize) -> HashMap<LeafKey, String> {
    let mut texts = HashMap::new();
    for (offset, block) in blocks.iter().enumerate() {
        block.for_each_segment_at(first_block + offset, &mut |path, line, text, _| {
            texts.insert((path.to_vec(), line), text.to_string());
        });
    }
    texts
}

/// The length of the text `new` shares with `old`, on a char boundary of `new`.
fn common_prefix_len(new: &str, old: &str) -> usize {
    let mut len = new
        .bytes()
        .zip(old.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !new.is_char_boundary(len) {
        len -= 1;
    }
    len
}

/// The start offset between consecutive units of an append `units` long:
/// the stagger, or none once the last unit would start after one fade has
/// played -- that is a sweep across text that arrived at once, not words
/// arriving one after another.
fn stagger_step(units: usize) -> Duration {
    if units < 2 || STAGGER * (units as u32 - 1) > FADE {
        return Duration::ZERO;
    }
    STAGGER
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
