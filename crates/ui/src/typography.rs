//! Text roles over gpui-base's `TypographyTokens`.
//!
//! gpui-base ships the scale (`xs` 12/16, `sm` 14/20, `md` 16/24, `lg` 18/28,
//! `xl` 20/28, `mono_md` 13/20) but no helper that applies a whole token to an
//! element, and no role names. This module is that one place: a view names the
//! role of a run of text and never picks a pixel size. A compact (touch)
//! layout reads one step up the same scale, except where a role says
//! otherwise; gpui-base has no touch scale, so that rule lives here and
//! nowhere else.

use crate::theme::ActiveTheme as _;
use gpui::{App, Hsla, Pixels, SharedString, Styled, Window};
use gpui_base::TextStyleToken;

pub trait TextRole: Styled + Sized {
    /// Size, line height and weight of one token.
    fn text_token(self, token: &TextStyleToken) -> Self {
        self.text_size(token.size)
            .line_height(token.line_height)
            .font_weight(token.weight)
    }

    /// Body text of a row, a form or a message: `sm`, `md` on a touch layout.
    fn text_body(self, compact: bool, cx: &App) -> Self {
        let typography = &cx.theme().tokens.typography;
        self.text_token(if compact {
            &typography.md
        } else {
            &typography.sm
        })
    }

    /// The label of a composer chip: `sm` on every layout. The chip is drawn
    /// 28 tall on a touch layout too, and the phone row has to fit a model
    /// name beside the send control, so it does not take the touch step up.
    fn text_chip(self, cx: &App) -> Self {
        self.text_token(&cx.theme().tokens.typography.sm)
    }

    /// A page, panel or sheet title: `md`, medium weight.
    fn text_title(self, cx: &App) -> Self {
        self.text_token(&cx.theme().tokens.typography.md)
            .font_weight(gpui::FontWeight::MEDIUM)
    }

    /// A caption that names a section or a field: `xs`, medium, muted.
    fn text_caption(self, cx: &App) -> Self {
        let theme = cx.theme();
        self.text_token(&theme.tokens.typography.xs)
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.muted_foreground)
    }

    /// A row's second line, a timestamp, a count: `xs`, regular, muted.
    /// `sm` on a touch layout.
    fn text_secondary(self, compact: bool, cx: &App) -> Self {
        let theme = cx.theme();
        let typography = &theme.tokens.typography;
        self.text_token(if compact {
            &typography.sm
        } else {
            &typography.xs
        })
        .text_color(theme.muted_foreground)
    }

    /// A hash, a path, a duration: [`TextRole::text_secondary`] in the mono family.
    fn text_meta_mono(self, compact: bool, cx: &App) -> Self {
        let mono = cx.theme().tokens.typography.mono.clone();
        self.text_secondary(compact, cx).font_family(mono)
    }

    /// Inline code: `mono_md` in the mono family.
    fn text_code(self, cx: &App) -> Self {
        let typography = &cx.theme().tokens.typography;
        self.text_token(&typography.mono_md)
            .font_family(typography.mono.clone())
    }
}

impl<T: Styled> TextRole for T {}

/// How far below the centre of its line box a line of `text`, in `family` at
/// `size`, paints its baseline: (ascent - descent) / 2 of the metrics shaping
/// finds, which take the largest over the fonts the line is drawn in. So it
/// depends on the platform's fonts and the locale's CJK fallback (on Linux,
/// Noto Sans CJK's ascent is taller than the monospace's), and is measured
/// rather than assumed.
///
/// A row of text in different sizes or fonts cannot be lined up with
/// `items_baseline`: gpui-fast's measured text leaves hand taffy a size and no
/// baseline, and taffy then takes a box's bottom edge for its baseline. The
/// row centres its lines instead and moves one onto another's baseline by the
/// difference of this between them, until gpui-fast reports text baselines.
pub(crate) fn baseline_below_centre(
    text: SharedString,
    family: SharedString,
    size: Pixels,
    window: &Window,
) -> Pixels {
    let run = gpui::TextRun {
        len: text.len(),
        font: gpui::Font {
            family,
            fallbacks: crate::i18n::cjk_font_fallbacks(),
            ..Default::default()
        },
        color: Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(text, size, &[run], None);
    line_below_centre(&line, window)
}

/// [`baseline_below_centre`] of a line already shaped.
pub(crate) fn line_below_centre(line: &gpui::ShapedLine, window: &Window) -> Pixels {
    // Glyphs have no vertical subpixel variants, so a baseline is painted on
    // a whole device pixel, as layout puts a box's offset on one: rounded
    // likewise, the difference of two of these moves a line exactly onto
    // another's baseline instead of a pixel short of it.
    let scale = window.scale_factor();
    ((line.ascent - line.descent) / 2. * scale).round() / scale
}
