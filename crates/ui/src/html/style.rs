//! Computed CSS styles, UA defaults, theme CSS custom properties, and
//! translation into GPUI [`StyleRefinement`].

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AbsoluteLength, AlignContent, AlignItems, AlignSelf, Background, BorderStyle, BoxShadow,
    DefiniteLength, Display, Fill, FlexDirection, FlexWrap, FontStyle, FontWeight, GridLocation,
    GridPlacement, GridTemplate, GridTemplateMinSize, Hsla, JustifyContent, Length, Overflow,
    Pixels, Position, SharedString, StrikethroughStyle, StyleRefinement, TextAlign, UnderlineStyle,
    Visibility, WhiteSpace, linear_color_stop, linear_gradient, point, px, relative, rems,
};

use super::color::{hsla_to_css, parse_color_for_mode, split_top_level_commas};
use super::css::{
    ActiveRules, Budget, Declaration, PseudoElement, Vars, number, parse_declarations, resolve_vars,
};
use super::dom::{Document, ElementData, NodeId};
use crate::theme::Theme;

/// Default base font size in pixels for `rem` and root `em` units.
pub(crate) const BASE_FONT_PX: f32 = 14.0;

/// The longest length a declaration may give, in pixels either way. Past it
/// a row's height stops being a number the timeline can add to the others.
const MAX_PX: f32 = 100_000.0;
/// The largest multiple of its container a percentage may give.
const MAX_FRACTION: f32 = 100.0;
/// The largest flex or `fr` factor: the factors of one line are summed.
const MAX_FACTOR: f32 = 10_000.0;
/// The most grid tracks a template or a placement may name; layout walks
/// every track for every item.
pub(crate) const MAX_GRID_TRACKS: u16 = 256;
/// The most shadows one box is painted with.
const MAX_SHADOWS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisplayKind {
    Block,
    Inline,
    InlineBlock,
    Flex,
    InlineFlex,
    Grid,
    InlineGrid,
    Table,
    TableRowGroup,
    TableRow,
    TableCell,
    ListItem,
    None,
}

impl DisplayKind {
    pub(crate) fn is_inline_like(self) -> bool {
        matches!(
            self,
            Self::Inline | Self::InlineBlock | Self::InlineFlex | Self::InlineGrid
        )
    }

    pub(crate) fn is_pure_inline(self) -> bool {
        matches!(self, Self::Inline)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextTransform {
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WhiteSpaceMode {
    Normal,
    NoWrap,
    Pre,
    PreWrap,
    PreLine,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GridTrack {
    Fr(f32),
    Px(f32),
    Percent(f32),
    Auto,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GridColumns {
    Equal(u16),
    AutoFit {
        min_px: f32,
    },
    /// As `AutoFit`, keeping the columns no item fills.
    AutoFill {
        min_px: f32,
    },
    Tracks(Vec<GridTrack>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListStyleType {
    Default,
    None,
    Disc,
    Circle,
    Square,
    Decimal,
}

/// Complete resolved style for one DOM element.
#[derive(Clone, Debug)]
pub(crate) struct ComputedStyle {
    // Inherited typography & custom properties
    pub(crate) color: Hsla,
    pub(crate) font_size: f32,
    pub(crate) font_weight: FontWeight,
    pub(crate) font_style: FontStyle,
    pub(crate) font_family: SharedString,
    pub(crate) line_height: Option<DefiniteLength>,
    pub(crate) text_align: Option<TextAlign>,
    pub(crate) text_transform: TextTransform,
    pub(crate) white_space: WhiteSpaceMode,
    pub(crate) list_style: ListStyleType,
    pub(crate) vars: Rc<Vars>,

    // Text decorations (not strictly inherited in CSS, carried on inline spans)
    pub(crate) underline: bool,
    pub(crate) line_through: bool,
    pub(crate) text_ellipsis: bool,

    // Display & visibility
    pub(crate) display: DisplayKind,
    pub(crate) visibility: Visibility,
    pub(crate) opacity: Option<f32>,

    // Positioning
    pub(crate) position: Position,
    pub(crate) top: Option<Length>,
    pub(crate) right: Option<Length>,
    pub(crate) bottom: Option<Length>,
    pub(crate) left: Option<Length>,

    // Sizing
    pub(crate) width: Option<Length>,
    pub(crate) height: Option<Length>,
    pub(crate) min_width: Option<Length>,
    pub(crate) min_height: Option<Length>,
    pub(crate) max_width: Option<Length>,
    pub(crate) max_height: Option<Length>,
    pub(crate) aspect_ratio: Option<f32>,

    // Box model
    pub(crate) margin_top: Option<Length>,
    pub(crate) margin_right: Option<Length>,
    pub(crate) margin_bottom: Option<Length>,
    pub(crate) margin_left: Option<Length>,
    pub(crate) margin_auto_x: bool,

    pub(crate) padding_top: Option<DefiniteLength>,
    pub(crate) padding_right: Option<DefiniteLength>,
    pub(crate) padding_bottom: Option<DefiniteLength>,
    pub(crate) padding_left: Option<DefiniteLength>,

    pub(crate) border_top_w: Option<AbsoluteLength>,
    pub(crate) border_right_w: Option<AbsoluteLength>,
    pub(crate) border_bottom_w: Option<AbsoluteLength>,
    pub(crate) border_left_w: Option<AbsoluteLength>,
    pub(crate) border_color: Option<Hsla>,
    pub(crate) border_top_color: Option<Hsla>,
    pub(crate) border_right_color: Option<Hsla>,
    pub(crate) border_bottom_color: Option<Hsla>,
    pub(crate) border_left_color: Option<Hsla>,
    pub(crate) border_style: BorderStyle,
    pub(crate) border_collapse: bool,

    pub(crate) radius_tl: Option<AbsoluteLength>,
    pub(crate) radius_tr: Option<AbsoluteLength>,
    pub(crate) radius_br: Option<AbsoluteLength>,
    pub(crate) radius_bl: Option<AbsoluteLength>,

    // Flex & Grid
    pub(crate) flex_direction: Option<FlexDirection>,
    pub(crate) flex_wrap: Option<FlexWrap>,
    pub(crate) flex_grow: Option<f32>,
    pub(crate) flex_shrink: Option<f32>,
    pub(crate) flex_basis: Option<Length>,
    pub(crate) justify_content: Option<JustifyContent>,
    pub(crate) align_items: Option<AlignItems>,
    pub(crate) align_self: Option<AlignSelf>,
    pub(crate) align_content: Option<AlignContent>,
    pub(crate) row_gap: Option<DefiniteLength>,
    pub(crate) col_gap: Option<DefiniteLength>,

    pub(crate) grid_columns: Option<GridColumns>,
    pub(crate) grid_rows: Option<u16>,
    pub(crate) grid_location: Option<GridLocation>,

    // Visuals & Overflow
    pub(crate) background: Option<Fill>,
    pub(crate) background_hsla: Option<Hsla>,
    pub(crate) box_shadows: Vec<BoxShadow>,
    pub(crate) overflow_x: Option<Overflow>,
    pub(crate) overflow_y: Option<Overflow>,

    // Pseudo-element static content
    pub(crate) content: Option<String>,
}

impl ComputedStyle {
    /// Root inherited style seeded from the active Tcode theme and stylesheet `:root` vars.
    fn root(theme: &Palette, vars: Rc<Vars>) -> Self {
        Self {
            color: theme.foreground,
            font_size: BASE_FONT_PX,
            font_weight: FontWeight::NORMAL,
            font_style: FontStyle::Normal,
            font_family: theme.font_sans.clone(),
            line_height: Some(DefiniteLength::Fraction(1.5)),
            text_align: None,
            text_transform: TextTransform::None,
            white_space: WhiteSpaceMode::Normal,
            list_style: ListStyleType::Default,
            vars,
            underline: false,
            line_through: false,
            text_ellipsis: false,
            display: DisplayKind::Block,
            visibility: Visibility::Visible,
            opacity: None,
            position: Position::Relative,
            top: None,
            right: None,
            bottom: None,
            left: None,
            width: None,
            height: None,
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            aspect_ratio: None,
            margin_top: None,
            margin_right: None,
            margin_bottom: None,
            margin_left: None,
            margin_auto_x: false,
            padding_top: None,
            padding_right: None,
            padding_bottom: None,
            padding_left: None,
            border_top_w: None,
            border_right_w: None,
            border_bottom_w: None,
            border_left_w: None,
            border_color: None,
            border_top_color: None,
            border_right_color: None,
            border_bottom_color: None,
            border_left_color: None,
            border_style: BorderStyle::Solid,
            border_collapse: false,
            radius_tl: None,
            radius_tr: None,
            radius_br: None,
            radius_bl: None,
            flex_direction: None,
            flex_wrap: None,
            flex_grow: None,
            flex_shrink: None,
            flex_basis: None,
            justify_content: None,
            align_items: None,
            align_self: None,
            align_content: None,
            row_gap: None,
            col_gap: None,
            grid_columns: None,
            grid_rows: None,
            grid_location: None,
            background: None,
            background_hsla: None,
            box_shadows: Vec::new(),
            overflow_x: None,
            overflow_y: None,
            content: None,
        }
    }

    fn inherit_from(parent: &Self) -> Self {
        Self {
            color: parent.color,
            font_size: parent.font_size,
            font_weight: parent.font_weight,
            font_style: parent.font_style,
            font_family: parent.font_family.clone(),
            line_height: parent.line_height,
            text_align: parent.text_align,
            text_transform: parent.text_transform,
            white_space: parent.white_space,
            list_style: parent.list_style,
            vars: parent.vars.clone(),
            underline: parent.underline,
            line_through: parent.line_through,
            text_ellipsis: false,
            display: DisplayKind::Block,
            visibility: parent.visibility,
            opacity: None,
            position: Position::Relative,
            top: None,
            right: None,
            bottom: None,
            left: None,
            width: None,
            height: None,
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            aspect_ratio: None,
            margin_top: None,
            margin_right: None,
            margin_bottom: None,
            margin_left: None,
            margin_auto_x: false,
            padding_top: None,
            padding_right: None,
            padding_bottom: None,
            padding_left: None,
            border_top_w: None,
            border_right_w: None,
            border_bottom_w: None,
            border_left_w: None,
            border_color: None,
            border_top_color: None,
            border_right_color: None,
            border_bottom_color: None,
            border_left_color: None,
            border_style: BorderStyle::Solid,
            border_collapse: parent.border_collapse,
            radius_tl: None,
            radius_tr: None,
            radius_br: None,
            radius_bl: None,
            flex_direction: None,
            flex_wrap: None,
            flex_grow: None,
            flex_shrink: None,
            flex_basis: None,
            justify_content: None,
            align_items: None,
            align_self: None,
            align_content: None,
            row_gap: None,
            col_gap: None,
            grid_columns: None,
            grid_rows: None,
            grid_location: None,
            background: None,
            background_hsla: None,
            box_shadows: Vec::new(),
            overflow_x: None,
            overflow_y: None,
            content: None,
        }
    }

    /// True when this element draws a box (padding, border, background, shadow,
    /// or explicit size) that cannot be flattened into a plain `StyledText` run.
    pub(crate) fn has_inline_box(&self) -> bool {
        self.padding_top.is_some()
            || self.padding_right.is_some()
            || self.padding_bottom.is_some()
            || self.padding_left.is_some()
            || self.border_top_w.is_some()
            || self.border_right_w.is_some()
            || self.border_bottom_w.is_some()
            || self.border_left_w.is_some()
            || self.radius_tl.is_some()
            || self.width.is_some()
            || self.height.is_some()
            || !self.box_shadows.is_empty()
            || self.margin_left.is_some()
            || self.margin_right.is_some()
            || (self.background.is_some() && self.background_hsla.is_none())
    }

    /// Apply this computed style onto a GPUI `StyleRefinement`.
    pub(crate) fn apply_to(&self, style: &mut StyleRefinement, viewport_width: f32) {
        self.apply_flow_to(style, viewport_width);
        self.apply_box_to(style);
    }

    /// How the box lays out what it holds.
    pub(crate) fn apply_flow_to(&self, style: &mut StyleRefinement, viewport_width: f32) {
        match self.display {
            DisplayKind::None => style.display = Some(Display::None),
            DisplayKind::Flex | DisplayKind::InlineFlex => {
                style.display = Some(Display::Flex);
                style.flex_direction = Some(self.flex_direction.unwrap_or(FlexDirection::Row));
            }
            DisplayKind::Grid | DisplayKind::InlineGrid => {
                if let Some(cols) = &self.grid_columns {
                    match cols {
                        GridColumns::Equal(n) => {
                            style.display = Some(Display::Grid);
                            style.grid_cols = Some(GridTemplate {
                                repeat: (*n).max(1),
                                min_size: GridTemplateMinSize::Zero,
                            });
                        }
                        GridColumns::AutoFit { min_px } | GridColumns::AutoFill { min_px } => {
                            let gap_px =
                                definite_to_px(self.col_gap, self.font_size).unwrap_or(12.0);
                            let count = (((viewport_width + gap_px) / (min_px.max(40.0) + gap_px))
                                .floor() as u16)
                                .max(1);
                            style.display = Some(Display::Grid);
                            style.grid_cols = Some(GridTemplate {
                                repeat: count,
                                min_size: GridTemplateMinSize::Zero,
                            });
                        }
                        GridColumns::Tracks(_) => {
                            // Non-uniform track lists are synthesized as flex rows by the renderer.
                            style.display = Some(Display::Flex);
                            style.flex_direction = Some(FlexDirection::Column);
                        }
                    }
                } else {
                    style.display = Some(Display::Grid);
                    style.grid_cols = Some(GridTemplate {
                        repeat: 1,
                        min_size: GridTemplateMinSize::Zero,
                    });
                }
                if let Some(rows) = self.grid_rows {
                    style.grid_rows = Some(GridTemplate {
                        repeat: rows.max(1),
                        min_size: GridTemplateMinSize::Zero,
                    });
                }
            }
            DisplayKind::InlineBlock => {
                style.display = Some(Display::Flex);
                style.flex_direction = Some(FlexDirection::Row);
                style.align_items = Some(AlignItems::Center);
            }
            DisplayKind::Block
            | DisplayKind::ListItem
            | DisplayKind::Table
            | DisplayKind::TableRowGroup
            | DisplayKind::TableRow
            | DisplayKind::TableCell
            | DisplayKind::Inline => {
                style.display = Some(Display::Flex);
                style.flex_direction = Some(FlexDirection::Column);
            }
        }

        if let Some(wrap) = self.flex_wrap {
            style.flex_wrap = Some(wrap);
        }
        if let Some(jc) = self.justify_content {
            style.justify_content = Some(jc);
        }
        if let Some(ai) = self.align_items {
            style.align_items = Some(ai);
        }
        if let Some(ac) = self.align_content {
            style.align_content = Some(ac);
        }
        if let Some(gap) = self.col_gap {
            style.gap.width = Some(gap);
        }
        if let Some(gap) = self.row_gap {
            style.gap.height = Some(gap);
        }
    }

    /// The box itself: where it sits in its parent, its size, its edges and
    /// the type it is set in.
    fn apply_box_to(&self, style: &mut StyleRefinement) {
        if self.visibility == Visibility::Hidden {
            style.visibility = Some(Visibility::Hidden);
        }
        if let Some(op) = self.opacity {
            style.opacity = Some(op);
        }

        if self.position == Position::Absolute {
            style.position = Some(Position::Absolute);
        }
        if self.top.is_some() {
            style.inset.top = self.top;
        }
        if self.right.is_some() {
            style.inset.right = self.right;
        }
        if self.bottom.is_some() {
            style.inset.bottom = self.bottom;
        }
        if self.left.is_some() {
            style.inset.left = self.left;
        }

        if self.width.is_some() {
            style.size.width = self.width;
        }
        if self.height.is_some() {
            style.size.height = self.height;
        }
        if self.min_width.is_some() {
            style.min_size.width = self.min_width;
        }
        if self.min_height.is_some() {
            style.min_size.height = self.min_height;
        }
        if self.max_width.is_some() {
            style.max_size.width = self.max_width;
        }
        if self.max_height.is_some() {
            style.max_size.height = self.max_height;
        }
        if self.aspect_ratio.is_some() {
            style.aspect_ratio = self.aspect_ratio;
        }

        if self.margin_top.is_some() {
            style.margin.top = self.margin_top;
        }
        if self.margin_right.is_some() {
            style.margin.right = self.margin_right;
        }
        if self.margin_bottom.is_some() {
            style.margin.bottom = self.margin_bottom;
        }
        if self.margin_left.is_some() {
            style.margin.left = self.margin_left;
        }
        if self.margin_auto_x {
            style.margin.left = Some(Length::Auto);
            style.margin.right = Some(Length::Auto);
        }

        if self.padding_top.is_some() {
            style.padding.top = self.padding_top;
        }
        if self.padding_right.is_some() {
            style.padding.right = self.padding_right;
        }
        if self.padding_bottom.is_some() {
            style.padding.bottom = self.padding_bottom;
        }
        if self.padding_left.is_some() {
            style.padding.left = self.padding_left;
        }

        if self.border_top_w.is_some() {
            style.border_widths.top = self.border_top_w;
        }
        if self.border_right_w.is_some() {
            style.border_widths.right = self.border_right_w;
        }
        if self.border_bottom_w.is_some() {
            style.border_widths.bottom = self.border_bottom_w;
        }
        if self.border_left_w.is_some() {
            style.border_widths.left = self.border_left_w;
        }
        // A box is drawn in one border colour, as GPUI draws it: the one
        // given for the whole box, else a side's.
        if let Some(color) = self
            .border_color
            .or(self.border_left_color)
            .or(self.border_top_color)
            .or(self.border_bottom_color)
            .or(self.border_right_color)
        {
            style.border_color = Some(color);
        } else if self.border_top_w.is_some()
            || self.border_right_w.is_some()
            || self.border_bottom_w.is_some()
            || self.border_left_w.is_some()
        {
            style.border_color = Some(self.color);
        }
        if self.border_style != BorderStyle::Solid {
            style.border_style = Some(self.border_style);
        }

        if self.radius_tl.is_some() {
            style.corner_radii.top_left = self.radius_tl;
        }
        if self.radius_tr.is_some() {
            style.corner_radii.top_right = self.radius_tr;
        }
        if self.radius_br.is_some() {
            style.corner_radii.bottom_right = self.radius_br;
        }
        if self.radius_bl.is_some() {
            style.corner_radii.bottom_left = self.radius_bl;
        }

        if let Some(grow) = self.flex_grow {
            style.flex_grow = Some(grow);
        }
        if let Some(shrink) = self.flex_shrink {
            style.flex_shrink = Some(shrink);
        }
        if let Some(basis) = self.flex_basis {
            style.flex_basis = Some(basis);
        }
        if let Some(aself) = self.align_self {
            style.align_self = Some(aself);
        }
        if let Some(loc) = &self.grid_location {
            style.grid_location = Some(loc.clone());
        }

        if let Some(bg) = &self.background {
            style.background = Some(bg.clone());
        }
        if !self.box_shadows.is_empty() {
            style.box_shadow = Some(self.box_shadows.clone());
        }
        // A box that scrolls is given a scroll area by the renderer; the
        // style only clips it.
        let clip = |overflow| match overflow {
            Overflow::Scroll => Overflow::Hidden,
            other => other,
        };
        if let Some(ox) = self.overflow_x {
            style.overflow.x = Some(clip(ox));
        }
        if let Some(oy) = self.overflow_y {
            style.overflow.y = Some(clip(oy));
        }

        // Typography
        style.text.color = Some(self.color);
        style.text.font_size = Some(AbsoluteLength::Pixels(px(self.font_size)));
        style.text.font_weight = Some(self.font_weight);
        style.text.font_style = Some(self.font_style);
        style.text.font_family = Some(self.font_family.clone());
        if let Some(lh) = self.line_height {
            style.text.line_height = Some(lh);
        }
        if let Some(ta) = self.text_align {
            style.text.text_align = Some(ta);
        }
        if self.underline {
            style.text.underline = Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(self.color),
                wavy: false,
            });
        }
        if self.line_through {
            style.text.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(self.color),
            });
        }
        if matches!(self.white_space, WhiteSpaceMode::NoWrap) {
            style.text.white_space = Some(WhiteSpace::Nowrap);
        }
        if self.text_ellipsis
            && matches!(self.white_space, WhiteSpaceMode::NoWrap)
            && matches!(self.overflow_x, Some(Overflow::Hidden))
        {
            style.text.text_overflow = Some(gpui::TextOverflow::Truncate(SharedString::from("…")));
        }
    }
}

/// What of the active theme a cascade reads: the colours and fonts behind
/// the UA defaults and the custom properties the tool documents to the
/// model. Two themes with the same palette cascade alike.
#[derive(Clone, PartialEq)]
pub(crate) struct Palette {
    pub(crate) dark: bool,
    foreground: Hsla,
    muted_foreground: Hsla,
    muted: Hsla,
    border: Hsla,
    link: Hsla,
    warning: Hsla,
    font_sans: SharedString,
    font_mono: SharedString,
    radius: Pixels,
    colors: [(&'static str, Hsla); 18],
}

impl Palette {
    pub(crate) fn of(theme: &Theme) -> Self {
        // The theme has no palette for a chart's series; these are theirs,
        // in the light theme and the dark one.
        let series =
            |light: u32, dark: u32| theme.by_mode(gpui::rgb(light).into(), gpui::rgb(dark).into());
        Self {
            dark: theme.mode.is_dark(),
            foreground: theme.foreground,
            muted_foreground: theme.muted_foreground,
            muted: theme.muted,
            border: theme.border,
            link: theme.link,
            warning: theme.warning,
            font_sans: theme.font_family.clone(),
            font_mono: theme.mono_font_family.clone(),
            radius: theme.tokens.radius.md,
            colors: [
                ("--background", theme.background),
                ("--foreground", theme.foreground),
                ("--card", theme.muted),
                ("--muted", theme.muted),
                ("--muted-foreground", theme.muted_foreground),
                ("--border", theme.border),
                ("--primary", theme.primary),
                ("--accent", theme.accent),
                ("--destructive", theme.danger),
                ("--success", theme.success),
                ("--warning", theme.warning),
                ("--info", theme.info),
                ("--chart-1", series(0x2563eb, 0x60a5fa)),
                ("--chart-2", series(0x0d9488, 0x2dd4bf)),
                ("--chart-3", series(0xd97706, 0xfbbf24)),
                ("--chart-4", series(0x7c3aed, 0xa78bfa)),
                ("--chart-5", series(0xdb2777, 0xf472b6)),
                ("--chart-6", series(0x16a34a, 0x4ade80)),
            ],
        }
    }

    pub(crate) fn font_sans(&self) -> &str {
        &self.font_sans
    }

    /// The custom properties `tcode_visualize` documents to the model.
    fn variables(&self) -> Rc<Vars> {
        let mut values: Vec<(&str, String)> = self
            .colors
            .iter()
            .map(|(name, color)| (*name, hsla_to_css(*color)))
            .collect();
        values.push(("--font-sans", self.font_sans.to_string()));
        values.push(("--font-mono", self.font_mono.to_string()));
        values.push(("--radius", format!("{}px", f32::from(self.radius))));
        let declared: Vec<(&str, &str)> = values
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        Vars::declare(&Rc::default(), &declared)
    }
}

/// One pass over a document's styles: every element's style from its
/// parent's, within a [`Budget`].
pub(crate) struct Cascade<'a> {
    pub(crate) doc: &'a Document,
    pub(crate) rules: ActiveRules<'a>,
    pub(crate) theme: &'a Palette,
    pub(crate) budget: Budget,
    /// A value past one of this module's limits was drawn at the limit.
    pub(crate) clamped: bool,
}

impl Cascade<'_> {
    pub(crate) fn root(&mut self) -> ComputedStyle {
        let declared = self.rules.root_properties();
        let vars = self.declare(&self.theme.variables(), &declared);
        ComputedStyle::root(self.theme, vars)
    }

    fn declare(&mut self, inherited: &Rc<Vars>, declared: &[(&str, &str)]) -> Rc<Vars> {
        let cost = declared.iter().map(|(_, value)| cost(value)).sum();
        if declared.is_empty() || !self.budget.spend(cost) {
            return inherited.clone();
        }
        Vars::declare(inherited, declared)
    }

    /// Resolve the computed style for `node_id`.
    pub(crate) fn element(
        &mut self,
        node_id: NodeId,
        el: &ElementData,
        parent: &ComputedStyle,
    ) -> ComputedStyle {
        let mut style = ComputedStyle::inherit_from(parent);
        apply_ua_defaults(&mut style, el, self.theme);

        let matched = self
            .rules
            .matching_declarations(self.doc, node_id, None, &mut self.budget);
        let inline = el
            .inline_style
            .as_deref()
            .map(parse_declarations)
            .unwrap_or_default();
        // Order: normal matched -> normal inline -> !important matched -> !important inline
        let ordered: Vec<&Declaration> = (matched.iter().copied().filter(|d| !d.important))
            .chain(inline.iter().filter(|d| !d.important))
            .chain(matched.iter().copied().filter(|d| d.important))
            .chain(inline.iter().filter(|d| d.important))
            .collect();
        self.apply(&mut style, &ordered, parent.font_size);

        // HTML presentation attributes on elements (width/height/align/bgcolor).
        apply_html_presentational_attrs(&mut style, el);

        style
    }

    /// Resolve `::before` or `::after` pseudo-element style if `content` is specified.
    pub(crate) fn pseudo(
        &mut self,
        node_id: NodeId,
        parent: &ComputedStyle,
        pseudo: PseudoElement,
    ) -> Option<ComputedStyle> {
        let matched =
            self.rules
                .matching_declarations(self.doc, node_id, Some(pseudo), &mut self.budget);
        if matched.is_empty() {
            return None;
        }
        let mut style = ComputedStyle::inherit_from(parent);
        style.display = DisplayKind::Inline;
        self.apply(&mut style, &matched, parent.font_size);
        style.content.as_ref()?;
        Some(style)
    }

    /// Custom properties first, so `var()` works whatever the order; then
    /// `color` and `font-size`, which `currentColor` and `em` in the rest
    /// resolve against.
    fn apply(&mut self, style: &mut ComputedStyle, ordered: &[&Declaration], parent_em: f32) {
        let declared: Vec<(&str, &str)> = ordered
            .iter()
            .filter(|decl| decl.name.starts_with("--"))
            .map(|decl| (decl.name.as_str(), decl.value.as_str()))
            .collect();
        style.vars = self.declare(&style.vars, &declared);
        let first = |decl: &&&Declaration| matches!(decl.name.as_str(), "color" | "font-size");
        let rest = ordered
            .iter()
            .filter(|decl| !decl.name.starts_with("--") && !first(decl));
        for decl in ordered.iter().filter(first).chain(rest) {
            if !self.budget.spend(cost(&decl.value)) {
                return;
            }
            apply_declaration(style, decl, parent_em, self.theme, &mut self.clamped);
        }
    }
}

/// What a declaration of `value` costs a [`Budget`]: one unit, and one more
/// for every sixteen bytes it has to be read through.
fn cost(value: &str) -> usize {
    1 + value.len() / 16
}

fn apply_ua_defaults(style: &mut ComputedStyle, el: &ElementData, theme: &Palette) {
    match el.tag.as_str() {
        "span" | "a" | "strong" | "b" | "em" | "i" | "u" | "s" | "del" | "strike" | "small"
        | "sub" | "sup" | "code" | "kbd" | "samp" | "mark" | "abbr" | "cite" | "time" | "q"
        | "label" | "var" | "wbr" => {
            style.display = DisplayKind::Inline;
        }
        "button" | "badge" | "progress" | "meter" | "input" | "select" | "img" | "svg" => {
            style.display = DisplayKind::InlineBlock;
        }
        "table" => {
            style.display = DisplayKind::Table;
            style.width = Some(relative(1.0).into());
            style.border_collapse = true;
        }
        "thead" | "tbody" | "tfoot" => {
            style.display = DisplayKind::TableRowGroup;
        }
        "tr" => {
            style.display = DisplayKind::TableRow;
        }
        "td" | "th" => {
            style.display = DisplayKind::TableCell;
            style.padding_top = Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(6.0))));
            style.padding_bottom = Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(6.0))));
            style.padding_left = Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(10.0))));
            style.padding_right = Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(10.0))));
            if el.tag == "th" {
                style.font_weight = FontWeight::SEMIBOLD;
                style.text_align = Some(TextAlign::Left);
            }
        }
        "li" => {
            style.display = DisplayKind::ListItem;
        }
        _ => {
            style.display = DisplayKind::Block;
        }
    }

    match el.tag.as_str() {
        "h1" => {
            style.font_size = BASE_FONT_PX * 1.5;
            style.font_weight = FontWeight::BOLD;
            style.margin_top = Some(px(4.0).into());
            style.margin_bottom = Some(px(8.0).into());
            style.line_height = Some(DefiniteLength::Fraction(1.25));
        }
        "h2" => {
            style.font_size = BASE_FONT_PX * 1.25;
            style.font_weight = FontWeight::SEMIBOLD;
            style.margin_top = Some(px(6.0).into());
            style.margin_bottom = Some(px(6.0).into());
            style.line_height = Some(DefiniteLength::Fraction(1.3));
        }
        "h3" => {
            style.font_size = BASE_FONT_PX * 1.1;
            style.font_weight = FontWeight::SEMIBOLD;
            style.margin_top = Some(px(4.0).into());
            style.margin_bottom = Some(px(4.0).into());
        }
        "h4" | "h5" | "h6" => {
            style.font_size = BASE_FONT_PX;
            style.font_weight = FontWeight::SEMIBOLD;
            style.margin_bottom = Some(px(4.0).into());
        }
        "p" => {
            style.margin_bottom = Some(px(8.0).into());
        }
        "ul" | "ol" => {
            style.padding_left = Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(18.0))));
            style.margin_bottom = Some(px(8.0).into());
            style.row_gap = Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(3.0))));
        }
        "strong" | "b" => {
            style.font_weight = FontWeight::SEMIBOLD;
        }
        "em" | "i" | "cite" => {
            style.font_style = FontStyle::Italic;
        }
        "u" => {
            style.underline = true;
        }
        "s" | "del" | "strike" => {
            style.line_through = true;
        }
        "small" | "sub" | "sup" => {
            style.font_size = (style.font_size * 0.85).max(10.0);
        }
        "a" => {
            style.color = theme.link;
            style.underline = true;
        }
        "code" | "kbd" | "samp" => {
            style.font_family = theme.font_mono.clone();
            style.font_size = (style.font_size * 0.92).max(11.0);
            let bg = theme.muted;
            style.background_hsla = Some(bg);
            style.background = Some(Fill::Color(bg.into()));
        }
        "pre" => {
            style.font_family = theme.font_mono.clone();
            style.font_size = (BASE_FONT_PX * 0.9).max(11.5);
            style.white_space = WhiteSpaceMode::Pre;
            style.padding_top = Some(px(10.0).into());
            style.padding_bottom = Some(px(10.0).into());
            style.padding_left = Some(px(12.0).into());
            style.padding_right = Some(px(12.0).into());
            style.radius_tl = Some(px(6.0).into());
            style.radius_tr = Some(px(6.0).into());
            style.radius_br = Some(px(6.0).into());
            style.radius_bl = Some(px(6.0).into());
            let bg = theme.muted;
            style.background_hsla = Some(bg);
            style.background = Some(Fill::Color(bg.into()));
            style.overflow_x = Some(Overflow::Scroll);
            style.margin_bottom = Some(px(8.0).into());
        }
        "mark" => {
            let bg = theme.warning.opacity(0.25);
            style.background_hsla = Some(bg);
            style.background = Some(Fill::Color(bg.into()));
        }
        "blockquote" => {
            style.border_left_w = Some(px(3.0).into());
            style.border_color = Some(theme.border);
            style.padding_left = Some(px(12.0).into());
            style.margin_bottom = Some(px(8.0).into());
            style.color = theme.muted_foreground;
        }
        "hr" => {
            style.height = Some(px(1.0).into());
            style.width = Some(relative(1.0).into());
            style.margin_top = Some(px(10.0).into());
            style.margin_bottom = Some(px(10.0).into());
            style.background_hsla = Some(theme.border);
            style.background = Some(Fill::Color(theme.border.into()));
        }
        "dt" => {
            style.font_weight = FontWeight::SEMIBOLD;
            style.margin_top = Some(px(6.0).into());
        }
        "dd" => {
            style.padding_left = Some(px(14.0).into());
            style.margin_bottom = Some(px(4.0).into());
            style.color = theme.muted_foreground;
        }
        _ => {}
    }
}

fn apply_html_presentational_attrs(style: &mut ComputedStyle, el: &ElementData) {
    if el.tag == "svg" || el.tag == "img" {
        if style.width.is_none()
            && let Some(w) = el
                .attr("width")
                .and_then(|s| parse_attr_length(s, style.font_size))
        {
            style.width = Some(w);
        }
        if style.height.is_none()
            && let Some(h) = el
                .attr("height")
                .and_then(|s| parse_attr_length(s, style.font_size))
        {
            style.height = Some(h);
        }
    }
    if style.text_align.is_none()
        && let Some(align) = el.attr("align")
    {
        style.text_align = parse_text_align(align);
    }
}

fn parse_attr_length(raw: &str, em: f32) -> Option<Length> {
    let s = raw.trim();
    if let Some(n) = number(s) {
        return unsigned(Some(Length::from(px(bounded_px(n)))));
    }
    unsigned(parse_length(s, em))
}

fn apply_declaration(
    style: &mut ComputedStyle,
    decl: &Declaration,
    parent_em: f32,
    theme: &Palette,
    clamped: &mut bool,
) {
    let Some(raw) = resolve_vars(&decl.value, &style.vars) else {
        return;
    };
    let val = raw.trim();
    if val.is_empty() {
        return;
    }
    let em = style.font_size;
    let dark = theme.dark;

    match decl.name.as_str() {
        "display" => {
            style.display = match val.to_ascii_lowercase().as_str() {
                "none" => DisplayKind::None,
                "block" | "flow-root" => DisplayKind::Block,
                "inline" => DisplayKind::Inline,
                "inline-block" => DisplayKind::InlineBlock,
                "flex" => DisplayKind::Flex,
                "inline-flex" => DisplayKind::InlineFlex,
                "grid" => DisplayKind::Grid,
                "inline-grid" => DisplayKind::InlineGrid,
                "table" => DisplayKind::Table,
                "table-header-group" | "table-row-group" | "table-footer-group" => {
                    DisplayKind::TableRowGroup
                }
                "table-row" => DisplayKind::TableRow,
                "table-cell" => DisplayKind::TableCell,
                "list-item" => DisplayKind::ListItem,
                _ => style.display,
            };
        }
        "visibility" => {
            if val.eq_ignore_ascii_case("hidden") || val.eq_ignore_ascii_case("collapse") {
                style.visibility = Visibility::Hidden;
            } else if val.eq_ignore_ascii_case("visible") {
                style.visibility = Visibility::Visible;
            }
        }
        "opacity" => {
            if let Some(pct) = val.strip_suffix('%') {
                if let Some(n) = number(pct) {
                    style.opacity = Some((n / 100.0).clamp(0.0, 1.0));
                }
            } else if let Some(n) = number(val) {
                style.opacity = Some(n.clamp(0.0, 1.0));
            }
        }
        "position" => match val.to_ascii_lowercase().as_str() {
            "absolute" | "fixed" => style.position = Position::Absolute,
            "relative" | "static" | "sticky" => style.position = Position::Relative,
            _ => {}
        },
        "top" => style.top = parse_length(val, em),
        "right" => style.right = parse_length(val, em),
        "bottom" => style.bottom = parse_length(val, em),
        "left" => style.left = parse_length(val, em),
        "inset" => {
            let parts = parse_four_lengths(val, em);
            if let Some([t, r, b, l]) = parts {
                style.top = Some(t);
                style.right = Some(r);
                style.bottom = Some(b);
                style.left = Some(l);
            }
        }
        "width" => style.width = unsigned(parse_length(val, em)),
        "height" => style.height = unsigned(parse_length(val, em)),
        "min-width" => style.min_width = unsigned(parse_length(val, em)),
        "min-height" => style.min_height = unsigned(parse_length(val, em)),
        "max-width" => {
            if val.eq_ignore_ascii_case("none") {
                style.max_width = None;
            } else {
                style.max_width = unsigned(parse_length(val, em));
            }
        }
        "max-height" => {
            if val.eq_ignore_ascii_case("none") {
                style.max_height = None;
            } else {
                style.max_height = unsigned(parse_length(val, em));
            }
        }
        "aspect-ratio" => {
            style.aspect_ratio = parse_aspect_ratio(val);
        }
        "margin" => {
            let tokens: Vec<&str> = val.split_whitespace().collect();
            if tokens.len() == 2 && tokens[1].eq_ignore_ascii_case("auto") {
                if let Some(v) = parse_length(tokens[0], em) {
                    style.margin_top = Some(v);
                    style.margin_bottom = Some(v);
                }
                style.margin_auto_x = true;
            } else if tokens.len() == 1 && tokens[0].eq_ignore_ascii_case("auto") {
                style.margin_auto_x = true;
            } else if let Some([t, r, b, l]) = parse_four_lengths(val, em) {
                style.margin_top = Some(t);
                style.margin_right = Some(r);
                style.margin_bottom = Some(b);
                style.margin_left = Some(l);
                style.margin_auto_x = r == Length::Auto && l == Length::Auto;
            }
        }
        "margin-top" => style.margin_top = parse_length(val, em),
        "margin-right" => style.margin_right = parse_length(val, em),
        "margin-bottom" => style.margin_bottom = parse_length(val, em),
        "margin-left" => style.margin_left = parse_length(val, em),
        "margin-inline" => {
            if let Some([l, r]) = parse_two_lengths(val, em) {
                style.margin_left = Some(l);
                style.margin_right = Some(r);
                style.margin_auto_x = l == Length::Auto && r == Length::Auto;
            }
        }
        "margin-block" => {
            if let Some([t, b]) = parse_two_lengths(val, em) {
                style.margin_top = Some(t);
                style.margin_bottom = Some(b);
            }
        }
        "padding" => {
            if let Some([t, r, b, l]) = all_unsigned(parse_four_definite(val, em)) {
                style.padding_top = Some(t);
                style.padding_right = Some(r);
                style.padding_bottom = Some(b);
                style.padding_left = Some(l);
            }
        }
        "padding-top" => style.padding_top = unsigned(parse_definite(val, em)),
        "padding-right" => style.padding_right = unsigned(parse_definite(val, em)),
        "padding-bottom" => style.padding_bottom = unsigned(parse_definite(val, em)),
        "padding-left" => style.padding_left = unsigned(parse_definite(val, em)),
        "padding-inline" => {
            if let Some([l, r]) = all_unsigned(parse_two_definite(val, em)) {
                style.padding_left = Some(l);
                style.padding_right = Some(r);
            }
        }
        "padding-block" => {
            if let Some([t, b]) = all_unsigned(parse_two_definite(val, em)) {
                style.padding_top = Some(t);
                style.padding_bottom = Some(b);
            }
        }
        "border" => {
            let (w, bs, color) = parse_border_shorthand(val, em, style.color, dark);
            if let Some(w) = w {
                style.border_top_w = Some(w);
                style.border_right_w = Some(w);
                style.border_bottom_w = Some(w);
                style.border_left_w = Some(w);
            }
            if let Some(bs) = bs {
                style.border_style = bs;
            }
            if let Some(c) = color {
                style.border_color = Some(c);
                style.border_top_color = None;
                style.border_right_color = None;
                style.border_bottom_color = None;
                style.border_left_color = None;
            }
        }
        "border-top" => apply_side_border(val, em, style, Side::Top, dark),
        "border-right" => apply_side_border(val, em, style, Side::Right, dark),
        "border-bottom" => apply_side_border(val, em, style, Side::Bottom, dark),
        "border-left" => apply_side_border(val, em, style, Side::Left, dark),
        "border-width" => {
            if let Some([t, r, b, l]) = all_unsigned(parse_four_absolute(val, em)) {
                style.border_top_w = Some(t);
                style.border_right_w = Some(r);
                style.border_bottom_w = Some(b);
                style.border_left_w = Some(l);
            }
        }
        "border-top-width" => style.border_top_w = parse_border_width(val, em),
        "border-right-width" => style.border_right_w = parse_border_width(val, em),
        "border-bottom-width" => style.border_bottom_w = parse_border_width(val, em),
        "border-left-width" => style.border_left_w = parse_border_width(val, em),
        "border-color" => {
            if let Some(c) = parse_color_for_mode(val, dark) {
                style.border_color = Some(c.resolve(style.color));
            }
        }
        "border-top-color" => {
            if let Some(c) = parse_color_for_mode(val, dark) {
                style.border_top_color = Some(c.resolve(style.color));
            }
        }
        "border-right-color" => {
            if let Some(c) = parse_color_for_mode(val, dark) {
                style.border_right_color = Some(c.resolve(style.color));
            }
        }
        "border-bottom-color" => {
            if let Some(c) = parse_color_for_mode(val, dark) {
                style.border_bottom_color = Some(c.resolve(style.color));
            }
        }
        "border-left-color" => {
            if let Some(c) = parse_color_for_mode(val, dark) {
                style.border_left_color = Some(c.resolve(style.color));
            }
        }
        "border-style" => {
            if let Some(bs) = parse_border_style(val) {
                style.border_style = bs;
            } else if val.eq_ignore_ascii_case("none") {
                style.border_top_w = Some(px(0.0).into());
                style.border_right_w = Some(px(0.0).into());
                style.border_bottom_w = Some(px(0.0).into());
                style.border_left_w = Some(px(0.0).into());
            }
        }
        "border-collapse" => {
            style.border_collapse = val.eq_ignore_ascii_case("collapse");
        }
        "border-radius" => {
            // Strip optional `/ ...` elliptical radii
            let main = val.split('/').next().unwrap_or(val).trim();
            if let Some([tl, tr, br, bl]) = all_unsigned(parse_four_radius(main, em)) {
                style.radius_tl = Some(tl);
                style.radius_tr = Some(tr);
                style.radius_br = Some(br);
                style.radius_bl = Some(bl);
            }
        }
        "border-top-left-radius" => style.radius_tl = parse_radius_value(val, em),
        "border-top-right-radius" => style.radius_tr = parse_radius_value(val, em),
        "border-bottom-right-radius" => style.radius_br = parse_radius_value(val, em),
        "border-bottom-left-radius" => style.radius_bl = parse_radius_value(val, em),

        // Flex
        "flex-direction" => {
            style.flex_direction = match val.to_ascii_lowercase().as_str() {
                "row" => Some(FlexDirection::Row),
                "row-reverse" => Some(FlexDirection::RowReverse),
                "column" => Some(FlexDirection::Column),
                "column-reverse" => Some(FlexDirection::ColumnReverse),
                _ => style.flex_direction,
            };
        }
        "flex-wrap" => {
            style.flex_wrap = match val.to_ascii_lowercase().as_str() {
                "wrap" => Some(FlexWrap::Wrap),
                "wrap-reverse" => Some(FlexWrap::WrapReverse),
                "nowrap" => Some(FlexWrap::NoWrap),
                _ => style.flex_wrap,
            };
        }
        "flex-flow" => {
            for tok in val.split_whitespace() {
                match tok.to_ascii_lowercase().as_str() {
                    "row" => style.flex_direction = Some(FlexDirection::Row),
                    "row-reverse" => style.flex_direction = Some(FlexDirection::RowReverse),
                    "column" => style.flex_direction = Some(FlexDirection::Column),
                    "column-reverse" => style.flex_direction = Some(FlexDirection::ColumnReverse),
                    "wrap" => style.flex_wrap = Some(FlexWrap::Wrap),
                    "wrap-reverse" => style.flex_wrap = Some(FlexWrap::WrapReverse),
                    "nowrap" => style.flex_wrap = Some(FlexWrap::NoWrap),
                    _ => {}
                }
            }
        }
        "flex-grow" => {
            if let Some(n) = factor(val) {
                style.flex_grow = Some(n);
            }
        }
        "flex-shrink" => {
            if let Some(n) = factor(val) {
                style.flex_shrink = Some(n);
            }
        }
        "flex-basis" => style.flex_basis = unsigned(parse_length(val, em)),
        "flex" => apply_flex_shorthand(style, val, em),

        // Alignment & gap
        "justify-content" => style.justify_content = parse_justify_content(val),
        "align-items" => style.align_items = parse_align_items(val),
        "align-self" => style.align_self = parse_align_self(val),
        "align-content" => style.align_content = parse_align_content(val),
        "place-items" => {
            let parts: Vec<&str> = val.split_whitespace().collect();
            if let Some(first) = parts.first() {
                style.align_items = parse_align_items(first);
                if let Some(jc) = parse_justify_content(parts.get(1).unwrap_or(first)) {
                    style.justify_content = Some(jc);
                }
            }
        }
        "place-content" => {
            let parts: Vec<&str> = val.split_whitespace().collect();
            if let Some(first) = parts.first() {
                style.align_content = parse_align_content(first);
                style.justify_content = parse_justify_content(parts.get(1).unwrap_or(first));
            }
        }
        "gap" => {
            if let Some([r, c]) = all_unsigned(parse_two_definite(val, em)) {
                style.row_gap = Some(r);
                style.col_gap = Some(c);
            }
        }
        "row-gap" => style.row_gap = unsigned(parse_definite(val, em)),
        "column-gap" => style.col_gap = unsigned(parse_definite(val, em)),

        // Grid
        "grid-template-columns" => {
            style.grid_columns = parse_grid_columns(val, em, clamped);
        }
        "grid-template-rows" => {
            style.grid_rows = parse_grid_row_count(val, clamped);
        }
        "grid-column" => {
            if let Some(range) = parse_grid_line_range(val, clamped) {
                let loc = style.grid_location.get_or_insert_default();
                loc.column = range;
            }
        }
        "grid-row" => {
            if let Some(range) = parse_grid_line_range(val, clamped) {
                let loc = style.grid_location.get_or_insert_default();
                loc.row = range;
            }
        }

        // Background & Shadows
        "background-color" => {
            if let Some(c) = parse_color_for_mode(val, dark) {
                let resolved = c.resolve(style.color);
                style.background_hsla = Some(resolved);
                style.background = Some(Fill::Color(resolved.into()));
            }
        }
        "background" | "background-image" => {
            if val.eq_ignore_ascii_case("none") {
                style.background = None;
                style.background_hsla = None;
            } else if let Some(grad) = parse_linear_gradient(val, style.color, dark) {
                style.background = Some(Fill::Color(grad));
                style.background_hsla = None;
            } else if let Some(c) = parse_color_for_mode(val, dark) {
                let resolved = c.resolve(style.color);
                style.background_hsla = Some(resolved);
                style.background = Some(Fill::Color(resolved.into()));
            } else {
                // Shorthand like `#fff url(...) no-repeat` — pick first valid color token.
                for tok in val.split_whitespace() {
                    if let Some(c) = parse_color_for_mode(tok, dark) {
                        let resolved = c.resolve(style.color);
                        style.background_hsla = Some(resolved);
                        style.background = Some(Fill::Color(resolved.into()));
                        break;
                    }
                }
            }
        }
        "box-shadow" => {
            if val.eq_ignore_ascii_case("none") {
                style.box_shadows.clear();
            } else {
                style.box_shadows = parse_box_shadows(val, em, style.color, dark);
                if style.box_shadows.len() > MAX_SHADOWS {
                    style.box_shadows.truncate(MAX_SHADOWS);
                    *clamped = true;
                }
            }
        }

        // Overflow
        "overflow" => {
            let parts: Vec<&str> = val.split_whitespace().collect();
            if let Some(first) = parts.first().and_then(|s| parse_overflow(s)) {
                style.overflow_x = Some(first);
                style.overflow_y = Some(
                    parts
                        .get(1)
                        .and_then(|s| parse_overflow(s))
                        .unwrap_or(first),
                );
            }
        }
        "overflow-x" => style.overflow_x = parse_overflow(val),
        "overflow-y" => style.overflow_y = parse_overflow(val),

        // Typography
        "color" => {
            if let Some(c) = parse_color_for_mode(val, dark) {
                style.color = c.resolve(style.color);
            }
        }
        "font-size" => {
            if let Some(sz) = parse_font_size(val, parent_em) {
                style.font_size = sz.clamp(8.0, 96.0);
            }
        }
        "font-weight" => {
            if let Some(w) = parse_font_weight(val) {
                style.font_weight = w;
            }
        }
        "font-style" => {
            style.font_style = match val.to_ascii_lowercase().as_str() {
                "italic" => FontStyle::Italic,
                "oblique" => FontStyle::Oblique,
                "normal" => FontStyle::Normal,
                _ => style.font_style,
            };
        }
        "font-family" => {
            let lower = val.to_ascii_lowercase();
            if lower.contains(&theme.font_mono.to_ascii_lowercase())
                || lower.contains("mono")
                || lower.contains("menlo")
                || lower.contains("consolas")
                || lower.contains("courier")
                || lower.contains("fira code")
                || lower.contains("jetbrains")
            {
                style.font_family = theme.font_mono.clone();
            } else {
                style.font_family = theme.font_sans.clone();
            }
        }
        "line-height" => {
            if val.eq_ignore_ascii_case("normal") {
                style.line_height = Some(DefiniteLength::Fraction(1.45));
            } else if let Some(mult) = number(val) {
                style.line_height = Some(DefiniteLength::Fraction(mult.clamp(0.8, 4.0)));
            } else if let Some(pct) = val.strip_suffix('%') {
                if let Some(n) = number(pct) {
                    style.line_height = Some(DefiniteLength::Fraction((n / 100.0).clamp(0.8, 4.0)));
                }
            } else if let Some(height) = parse_px_f32(val, em) {
                style.line_height = Some(px(height.clamp(0.8 * em, 4.0 * em)).into());
            }
        }
        "text-align" => {
            if let Some(ta) = parse_text_align(val) {
                style.text_align = Some(ta);
            }
        }
        "text-decoration" | "text-decoration-line" => {
            let lower = val.to_ascii_lowercase();
            if lower.contains("none") {
                style.underline = false;
                style.line_through = false;
            } else {
                if lower.contains("underline") {
                    style.underline = true;
                }
                if lower.contains("line-through") {
                    style.line_through = true;
                }
            }
        }
        "text-transform" => {
            style.text_transform = match val.to_ascii_lowercase().as_str() {
                "uppercase" => TextTransform::Uppercase,
                "lowercase" => TextTransform::Lowercase,
                "capitalize" => TextTransform::Capitalize,
                "none" => TextTransform::None,
                _ => style.text_transform,
            };
        }
        "white-space" => {
            style.white_space = match val.to_ascii_lowercase().as_str() {
                "nowrap" => WhiteSpaceMode::NoWrap,
                "pre" => WhiteSpaceMode::Pre,
                "pre-wrap" | "break-spaces" => WhiteSpaceMode::PreWrap,
                "pre-line" => WhiteSpaceMode::PreLine,
                "normal" => WhiteSpaceMode::Normal,
                _ => style.white_space,
            };
        }
        "text-overflow" => {
            style.text_ellipsis = val.eq_ignore_ascii_case("ellipsis");
        }
        "list-style" | "list-style-type" => {
            let lower = val.to_ascii_lowercase();
            if lower.contains("none") {
                style.list_style = ListStyleType::None;
            } else if lower.contains("decimal") {
                style.list_style = ListStyleType::Decimal;
            } else if lower.contains("square") {
                style.list_style = ListStyleType::Square;
            } else if lower.contains("circle") {
                style.list_style = ListStyleType::Circle;
            } else if lower.contains("disc") {
                style.list_style = ListStyleType::Disc;
            }
        }
        "content" => {
            style.content = parse_css_string(val);
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

fn apply_side_border(val: &str, em: f32, style: &mut ComputedStyle, side: Side, dark: bool) {
    let (w, bs, color) = parse_border_shorthand(val, em, style.color, dark);
    if let Some(w) = w {
        match side {
            Side::Top => style.border_top_w = Some(w),
            Side::Right => style.border_right_w = Some(w),
            Side::Bottom => style.border_bottom_w = Some(w),
            Side::Left => style.border_left_w = Some(w),
        }
    }
    if let Some(bs) = bs {
        style.border_style = bs;
    }
    if let Some(c) = color {
        match side {
            Side::Top => style.border_top_color = Some(c),
            Side::Right => style.border_right_color = Some(c),
            Side::Bottom => style.border_bottom_color = Some(c),
            Side::Left => style.border_left_color = Some(c),
        }
    }
}

fn parse_border_shorthand(
    val: &str,
    em: f32,
    current: Hsla,
    dark: bool,
) -> (Option<AbsoluteLength>, Option<BorderStyle>, Option<Hsla>) {
    if val.eq_ignore_ascii_case("none") || val.eq_ignore_ascii_case("0") {
        return (Some(px(0.0).into()), None, None);
    }
    let mut width = None;
    let mut style = None;
    let mut color = None;
    for tok in split_whitespace_preserving_parens(val) {
        if tok.eq_ignore_ascii_case("none") || tok.eq_ignore_ascii_case("hidden") {
            return (Some(px(0.0).into()), None, None);
        }
        if style.is_none()
            && let Some(bs) = parse_border_style(tok)
        {
            style = Some(bs);
            continue;
        }
        if width.is_none()
            && let Some(w) = parse_border_width(tok, em)
        {
            width = Some(w);
            continue;
        }
        if color.is_none()
            && let Some(c) = parse_color_for_mode(tok, dark)
        {
            color = Some(c.resolve(current));
        }
    }
    if style.is_some() && width.is_none() {
        width = Some(px(1.0).into());
    }
    (width, style, color)
}

fn parse_border_style(s: &str) -> Option<BorderStyle> {
    match s.to_ascii_lowercase().as_str() {
        "solid" | "double" | "groove" | "ridge" | "inset" | "outset" => Some(BorderStyle::Solid),
        "dashed" | "dotted" => Some(BorderStyle::Dashed),
        _ => None,
    }
}

fn parse_border_width(s: &str, em: f32) -> Option<AbsoluteLength> {
    match s.to_ascii_lowercase().as_str() {
        "thin" => Some(px(1.0).into()),
        "medium" => Some(px(2.0).into()),
        "thick" => Some(px(3.0).into()),
        _ => unsigned(parse_absolute(s, em)),
    }
}

/// A flex factor.
fn factor(raw: &str) -> Option<f32> {
    number(raw).map(|n| n.clamp(0.0, MAX_FACTOR))
}

trait Signed {
    fn is_negative(&self) -> bool;
}

impl Signed for AbsoluteLength {
    fn is_negative(&self) -> bool {
        match self {
            Self::Pixels(pixels) => *pixels < px(0.0),
            Self::Rems(rems) => rems.0 < 0.0,
        }
    }
}

impl Signed for DefiniteLength {
    fn is_negative(&self) -> bool {
        match self {
            Self::Absolute(length) => length.is_negative(),
            Self::Fraction(fraction) => *fraction < 0.0,
        }
    }
}

impl Signed for Length {
    fn is_negative(&self) -> bool {
        match self {
            Self::Definite(length) => length.is_negative(),
            Self::Auto => false,
        }
    }
}

/// A length for a property CSS gives no negative value: a size, a padding,
/// a border, a gap.
fn unsigned<T: Signed>(length: Option<T>) -> Option<T> {
    length.filter(|length| !length.is_negative())
}

fn all_unsigned<T: Signed, const N: usize>(lengths: Option<[T; N]>) -> Option<[T; N]> {
    lengths.filter(|lengths| lengths.iter().all(|length| !length.is_negative()))
}

/// A length in pixels as layout can take it: `em` and `calc()` multiply, so
/// a product may be out of range where each number read was not.
fn bounded_px(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(-MAX_PX, MAX_PX)
    }
}

fn bounded_fraction(percent: f32) -> f32 {
    (percent / 100.0).clamp(-MAX_FRACTION, MAX_FRACTION)
}

fn apply_flex_shorthand(style: &mut ComputedStyle, val: &str, em: f32) {
    let lower = val.to_ascii_lowercase();
    match lower.as_str() {
        "none" => {
            style.flex_grow = Some(0.0);
            style.flex_shrink = Some(0.0);
            style.flex_basis = Some(Length::Auto);
            return;
        }
        "auto" => {
            style.flex_grow = Some(1.0);
            style.flex_shrink = Some(1.0);
            style.flex_basis = Some(Length::Auto);
            return;
        }
        "initial" => {
            style.flex_grow = Some(0.0);
            style.flex_shrink = Some(1.0);
            style.flex_basis = Some(Length::Auto);
            return;
        }
        _ => {}
    }
    let parts: Vec<&str> = val.split_whitespace().collect();
    match parts.as_slice() {
        [one] => {
            if let Some(grow) = factor(one) {
                style.flex_grow = Some(grow);
                style.flex_shrink = Some(1.0);
                style.flex_basis = Some(px(0.0).into());
            } else if let Some(basis) = unsigned(parse_length(one, em)) {
                style.flex_grow = Some(1.0);
                style.flex_shrink = Some(1.0);
                style.flex_basis = Some(basis);
            }
        }
        [a, b] => {
            if let Some(grow) = factor(a) {
                style.flex_grow = Some(grow);
                if let Some(shrink) = factor(b) {
                    style.flex_shrink = Some(shrink);
                    style.flex_basis = Some(px(0.0).into());
                } else if let Some(basis) = unsigned(parse_length(b, em)) {
                    style.flex_shrink = Some(1.0);
                    style.flex_basis = Some(basis);
                }
            }
        }
        [a, b, c, ..] => {
            if let Some(grow) = factor(a) {
                style.flex_grow = Some(grow);
            }
            if let Some(shrink) = factor(b) {
                style.flex_shrink = Some(shrink);
            }
            if let Some(basis) = unsigned(parse_length(c, em)) {
                style.flex_basis = Some(basis);
            }
        }
        [] => {}
    }
}

fn parse_justify_content(val: &str) -> Option<JustifyContent> {
    Some(match val.to_ascii_lowercase().as_str() {
        "flex-start" | "start" | "left" => JustifyContent::Start,
        "flex-end" | "end" | "right" => JustifyContent::End,
        "center" => JustifyContent::Center,
        "space-between" => JustifyContent::SpaceBetween,
        "space-around" => JustifyContent::SpaceAround,
        "space-evenly" => JustifyContent::SpaceEvenly,
        "stretch" => JustifyContent::Stretch,
        _ => return None,
    })
}

fn parse_align_items(val: &str) -> Option<AlignItems> {
    Some(match val.to_ascii_lowercase().as_str() {
        "flex-start" | "start" => AlignItems::FlexStart,
        "flex-end" | "end" => AlignItems::FlexEnd,
        "center" => AlignItems::Center,
        "baseline" => AlignItems::Baseline,
        "stretch" => AlignItems::Stretch,
        _ => return None,
    })
}

fn parse_align_self(val: &str) -> Option<AlignSelf> {
    Some(match val.to_ascii_lowercase().as_str() {
        "flex-start" | "start" => AlignSelf::FlexStart,
        "flex-end" | "end" => AlignSelf::FlexEnd,
        "center" => AlignSelf::Center,
        "baseline" => AlignSelf::Baseline,
        "stretch" => AlignSelf::Stretch,
        _ => return None,
    })
}

fn parse_align_content(val: &str) -> Option<AlignContent> {
    Some(match val.to_ascii_lowercase().as_str() {
        "flex-start" | "start" => AlignContent::FlexStart,
        "flex-end" | "end" => AlignContent::FlexEnd,
        "center" => AlignContent::Center,
        "space-between" => AlignContent::SpaceBetween,
        "space-around" => AlignContent::SpaceAround,
        "space-evenly" => AlignContent::SpaceEvenly,
        "stretch" => AlignContent::Stretch,
        _ => return None,
    })
}

fn parse_overflow(val: &str) -> Option<Overflow> {
    Some(match val.to_ascii_lowercase().as_str() {
        "visible" => Overflow::Visible,
        "hidden" | "clip" => Overflow::Hidden,
        "scroll" | "auto" => Overflow::Scroll,
        _ => return None,
    })
}

fn parse_text_align(val: &str) -> Option<TextAlign> {
    Some(match val.to_ascii_lowercase().as_str() {
        "left" | "start" => TextAlign::Left,
        "center" => TextAlign::Center,
        "right" | "end" => TextAlign::Right,
        _ => return None,
    })
}

fn parse_font_weight(val: &str) -> Option<FontWeight> {
    match val.to_ascii_lowercase().as_str() {
        "thin" | "100" => Some(FontWeight::THIN),
        "extralight" | "200" => Some(FontWeight::EXTRA_LIGHT),
        "light" | "300" => Some(FontWeight::LIGHT),
        "normal" | "regular" | "400" => Some(FontWeight::NORMAL),
        "medium" | "500" => Some(FontWeight::MEDIUM),
        "semibold" | "600" => Some(FontWeight::SEMIBOLD),
        "bold" | "bolder" | "700" => Some(FontWeight::BOLD),
        "extrabold" | "800" => Some(FontWeight::EXTRA_BOLD),
        "black" | "900" => Some(FontWeight::BLACK),
        "lighter" => Some(FontWeight::LIGHT),
        other => number(other).map(|weight| FontWeight(weight.clamp(1.0, 1000.0))),
    }
}

fn parse_font_size(val: &str, parent_em: f32) -> Option<f32> {
    match val.to_ascii_lowercase().as_str() {
        "xx-small" => return Some(10.0),
        "x-small" => return Some(11.0),
        "small" => return Some(12.0),
        "medium" => return Some(BASE_FONT_PX),
        "large" => return Some(16.0),
        "x-large" => return Some(20.0),
        "xx-large" => return Some(24.0),
        "smaller" => return Some((parent_em * 0.85).max(10.0)),
        "larger" => return Some(parent_em * 1.2),
        _ => {}
    }
    if let Some(pct) = val.strip_suffix('%') {
        return number(pct).map(|n| bounded_px(parent_em * n / 100.0));
    }
    parse_px_f32(val, parent_em)
}

fn parse_aspect_ratio(val: &str) -> Option<f32> {
    let ratio = match val.split_once('/') {
        Some((w, h)) => number(w)? / number(h)?,
        None => number(val)?,
    };
    (ratio > 0.0).then(|| ratio.clamp(0.01, 100.0))
}

pub(crate) fn parse_length(raw: &str, em: f32) -> Option<Length> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("auto") || s.eq_ignore_ascii_case("fit-content") {
        return Some(Length::Auto);
    }
    parse_definite(s, em).map(Length::Definite)
}

pub(crate) fn parse_definite(raw: &str, em: f32) -> Option<DefiniteLength> {
    let s = raw.trim();
    match strip_calc(s) {
        Some(inner) => eval_calc(inner, em),
        None => parse_term(s, em),
    }
}

/// One length, which is not a `calc()`: a `calc()` holds terms, not itself.
fn parse_term(s: &str, em: f32) -> Option<DefiniteLength> {
    if let Some(pct) = s.strip_suffix('%') {
        return number(pct).map(|n| DefiniteLength::Fraction(bounded_fraction(n)));
    }
    if let Some(vw) = s.strip_suffix("vw") {
        return number(vw).map(|n| DefiniteLength::Fraction((n / 100.0).clamp(0.0, 1.0)));
    }
    parse_absolute(s, em).map(DefiniteLength::Absolute)
}

pub(crate) fn parse_absolute(raw: &str, em: f32) -> Option<AbsoluteLength> {
    let s = raw.trim();
    if let Some(r) = s.strip_suffix("rem") {
        return number(r).map(|n| rems(bounded_px(n * BASE_FONT_PX) / BASE_FONT_PX).into());
    }
    parse_px_f32(s, em).map(|v| AbsoluteLength::Pixels(px(v)))
}

pub(crate) fn parse_px_f32(raw: &str, em: f32) -> Option<f32> {
    let s = raw.trim();
    let (value, unit) = if s == "0" {
        (0.0, 1.0)
    } else if let Some(v) = s.strip_suffix("px") {
        (number(v)?, 1.0)
    } else if let Some(v) = s.strip_suffix("rem") {
        (number(v)?, BASE_FONT_PX)
    } else if let Some(v) = s.strip_suffix("em") {
        (number(v)?, em)
    } else if let Some(v) = s.strip_suffix("ch") {
        (number(v)?, em * 0.6)
    } else if let Some(v) = s.strip_suffix("pt") {
        (number(v)?, 1.3333)
    } else {
        (number(s.strip_suffix("vh")?)?, 6.0)
    };
    Some(bounded_px(value * unit))
}

fn definite_to_px(len: Option<DefiniteLength>, em: f32) -> Option<f32> {
    match len? {
        DefiniteLength::Absolute(AbsoluteLength::Pixels(p)) => Some(f32::from(p)),
        DefiniteLength::Absolute(AbsoluteLength::Rems(r)) => Some(r.0 * em),
        DefiniteLength::Fraction(_) => None,
    }
}

fn strip_calc(s: &str) -> Option<&str> {
    let lower_prefix = s.get(..5)?;
    if lower_prefix.eq_ignore_ascii_case("calc(") && s.ends_with(')') {
        Some(s[5..s.len() - 1].trim())
    } else {
        None
    }
}

/// Evaluate simple `calc(a + b)` / `calc(a - b)` expressions.
fn eval_calc(expr: &str, em: f32) -> Option<DefiniteLength> {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    if tokens.len() == 1 {
        return parse_term(tokens[0], em);
    }
    if tokens.len() == 3 {
        let op = tokens[1];
        if let (Some(a), Some(b)) = (parse_px_f32(tokens[0], em), parse_px_f32(tokens[2], em)) {
            let res = match op {
                "+" => a + b,
                "-" => a - b,
                "*" => a * b,
                "/" if b.abs() > 1e-6 => a / b,
                _ => return None,
            };
            return Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(
                bounded_px(res),
            ))));
        }
        if let (Some(a), Some(b)) = (
            tokens[0].strip_suffix('%').and_then(number),
            tokens[2].strip_suffix('%').and_then(number),
        ) {
            let res = match op {
                "+" => a + b,
                "-" => a - b,
                _ => return None,
            };
            return Some(DefiniteLength::Fraction(bounded_fraction(res)));
        }
        // Mixed `100% - 16px` fallback: preserve percentage fraction for container sizing.
        if let Some(a) = tokens[0].strip_suffix('%').and_then(number) {
            return Some(DefiniteLength::Fraction(bounded_fraction(a)));
        }
    }
    None
}

fn parse_radius_value(raw: &str, em: f32) -> Option<AbsoluteLength> {
    let s = raw.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let n = number(pct)?;
        return if n >= 49.0 {
            Some(px(9999.0).into())
        } else {
            unsigned(Some(px(n * 0.5).into()))
        };
    }
    unsigned(parse_absolute(s, em))
}

fn expand_four<T: Copy>(vals: &[T]) -> Option<[T; 4]> {
    match vals {
        [a] => Some([*a, *a, *a, *a]),
        [v, h] => Some([*v, *h, *v, *h]),
        [t, h, b] => Some([*t, *h, *b, *h]),
        [t, r, b, l, ..] => Some([*t, *r, *b, *l]),
        [] => None,
    }
}

fn parse_four_lengths(val: &str, em: f32) -> Option<[Length; 4]> {
    let parsed: Vec<Length> = split_whitespace_preserving_parens(val)
        .into_iter()
        .filter_map(|t| parse_length(t, em))
        .collect();
    expand_four(&parsed)
}

fn parse_two_lengths(val: &str, em: f32) -> Option<[Length; 2]> {
    let parsed: Vec<Length> = split_whitespace_preserving_parens(val)
        .into_iter()
        .filter_map(|t| parse_length(t, em))
        .collect();
    match parsed.as_slice() {
        [a] => Some([*a, *a]),
        [a, b, ..] => Some([*a, *b]),
        [] => None,
    }
}

fn parse_four_definite(val: &str, em: f32) -> Option<[DefiniteLength; 4]> {
    let parsed: Vec<DefiniteLength> = split_whitespace_preserving_parens(val)
        .into_iter()
        .filter_map(|t| parse_definite(t, em))
        .collect();
    expand_four(&parsed)
}

fn parse_two_definite(val: &str, em: f32) -> Option<[DefiniteLength; 2]> {
    let parsed: Vec<DefiniteLength> = split_whitespace_preserving_parens(val)
        .into_iter()
        .filter_map(|t| parse_definite(t, em))
        .collect();
    match parsed.as_slice() {
        [a] => Some([*a, *a]),
        [a, b, ..] => Some([*a, *b]),
        [] => None,
    }
}

fn parse_four_absolute(val: &str, em: f32) -> Option<[AbsoluteLength; 4]> {
    let parsed: Vec<AbsoluteLength> = split_whitespace_preserving_parens(val)
        .into_iter()
        .filter_map(|t| parse_border_width(t, em))
        .collect();
    expand_four(&parsed)
}

fn parse_four_radius(val: &str, em: f32) -> Option<[AbsoluteLength; 4]> {
    let parsed: Vec<AbsoluteLength> = split_whitespace_preserving_parens(val)
        .into_iter()
        .filter_map(|t| parse_radius_value(t, em))
        .collect();
    expand_four(&parsed)
}

/// A count of grid tracks, at most [`MAX_GRID_TRACKS`].
fn track_count(raw: &str, clamped: &mut bool) -> Option<u16> {
    let count = raw.trim().parse::<u64>().ok().filter(|n| *n > 0)?;
    *clamped |= count > u64::from(MAX_GRID_TRACKS);
    Some(count.min(u64::from(MAX_GRID_TRACKS)) as u16)
}

/// Parse `grid-template-columns` supporting `repeat(N, 1fr)`,
/// `repeat(auto-fit|auto-fill, minmax(Wpx, 1fr))`, and explicit track lists.
fn parse_grid_columns(val: &str, em: f32, clamped: &mut bool) -> Option<GridColumns> {
    let s = val.trim();
    if s.eq_ignore_ascii_case("none") {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    if let Some(inner) = lower
        .strip_prefix("repeat(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts = split_top_level_commas(inner);
        if parts.len() == 2 {
            let count = parts[0].trim();
            let track = parts[1].trim();
            if count == "auto-fit" || count == "auto-fill" {
                let min_px = extract_minmax_min(track, em).unwrap_or(180.0);
                return Some(if count == "auto-fit" {
                    GridColumns::AutoFit { min_px }
                } else {
                    GridColumns::AutoFill { min_px }
                });
            }
            if let Some(n) = track_count(count, clamped) {
                return Some(GridColumns::Equal(n));
            }
        }
    }
    let tokens = split_whitespace_preserving_parens(s);
    if tokens.is_empty() {
        return None;
    }
    let most = usize::from(MAX_GRID_TRACKS);
    let mut tracks = Vec::new();
    for tok in tokens {
        if tracks.len() >= most {
            *clamped = true;
            break;
        }
        let tl = tok.to_ascii_lowercase();
        if let Some(inner) = tl.strip_prefix("repeat(").and_then(|r| r.strip_suffix(')')) {
            let parts = split_top_level_commas(inner);
            if parts.len() == 2
                && let Some(n) = track_count(parts[0], clamped)
            {
                let room = most - tracks.len();
                *clamped |= usize::from(n) > room;
                let tr = parse_single_track(parts[1].trim(), em);
                tracks.extend(std::iter::repeat_n(tr, usize::from(n).min(room)));
                continue;
            }
        }
        tracks.push(parse_single_track(tok, em));
    }
    // Collapse uniform `fr` tracks directly into GPUI native Grid.
    let all_equal_fr = tracks
        .first()
        .and_then(|t| match t {
            GridTrack::Fr(v) => Some(*v),
            _ => None,
        })
        .is_some_and(|first| {
            tracks
                .iter()
                .all(|t| matches!(t, GridTrack::Fr(v) if (v - first).abs() < 1e-3))
        });
    if all_equal_fr {
        Some(GridColumns::Equal(tracks.len() as u16))
    } else {
        Some(GridColumns::Tracks(tracks))
    }
}

fn extract_minmax_min(track: &str, em: f32) -> Option<f32> {
    if let Some(inner) = track
        .strip_prefix("minmax(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts = split_top_level_commas(inner);
        if let Some(first) = parts.first() {
            return parse_px_f32(first.trim(), em);
        }
    }
    parse_px_f32(track, em)
}

fn parse_single_track(tok: &str, em: f32) -> GridTrack {
    let t = tok.trim();
    if let Some(inner) = t
        .to_ascii_lowercase()
        .strip_prefix("minmax(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts = split_top_level_commas(inner);
        if let Some(max_part) = parts.get(1) {
            return parse_track_size(max_part, em);
        }
    }
    parse_track_size(t, em)
}

/// One track size, which is not a `minmax()`: a `minmax()` holds sizes, not
/// itself.
fn parse_track_size(tok: &str, em: f32) -> GridTrack {
    let t = tok.trim();
    if let Some(fr) = t.strip_suffix("fr").and_then(number) {
        return GridTrack::Fr(fr.clamp(0.1, MAX_FACTOR));
    }
    if let Some(pct) = t.strip_suffix('%').and_then(number) {
        return GridTrack::Percent(pct / 100.0);
    }
    if let Some(p) = parse_px_f32(t, em) {
        return GridTrack::Px(p.max(0.0));
    }
    GridTrack::Auto
}

fn parse_grid_row_count(val: &str, clamped: &mut bool) -> Option<u16> {
    let lower = val.trim().to_ascii_lowercase();
    if let Some(inner) = lower
        .strip_prefix("repeat(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts = split_top_level_commas(inner);
        if let Some(n) = parts.first().and_then(|s| track_count(s, clamped)) {
            return Some(n);
        }
    }
    let count = split_whitespace_preserving_parens(val).len();
    track_count(&count.to_string(), clamped)
}

fn parse_grid_line_range(val: &str, clamped: &mut bool) -> Option<Range<GridPlacement>> {
    let mut parse_one = |s: &str| -> GridPlacement {
        let t = s.trim().to_ascii_lowercase();
        if let Some(rest) = t.strip_prefix("span")
            && let Some(n) = track_count(rest, clamped)
        {
            return GridPlacement::Span(n);
        }
        if let Ok(line) = t.parse::<i64>() {
            // A grid of that many tracks has one more line than tracks.
            let last = i64::from(MAX_GRID_TRACKS) + 1;
            *clamped |= line.abs() > last;
            return GridPlacement::Line(line.clamp(-last, last) as i16);
        }
        GridPlacement::Auto
    };
    if let Some((start, end)) = val.split_once('/') {
        let s = parse_one(start);
        let e = parse_one(end);
        Some(s..e)
    } else {
        let p = parse_one(val);
        match p {
            GridPlacement::Span(n) => Some(GridPlacement::Auto..GridPlacement::Span(n)),
            GridPlacement::Line(l) => Some(GridPlacement::Line(l)..GridPlacement::Auto),
            GridPlacement::Auto => None,
        }
    }
}

fn parse_linear_gradient(val: &str, current: Hsla, dark: bool) -> Option<Background> {
    let lower = val.to_ascii_lowercase();
    let open = lower.find("linear-gradient(")?;
    let rest = &val[open + "linear-gradient(".len()..];
    let mut depth = 1u32;
    let mut close = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = &rest[..close?];
    let parts = split_top_level_commas(inner);
    if parts.len() < 2 {
        return None;
    }

    let mut angle = 180.0f32; // CSS default: `to bottom`
    let mut start_idx = 0usize;
    if let Some(dir) = parse_gradient_direction(parts[0]) {
        angle = dir;
        start_idx = 1;
    }

    let stops: Vec<(Hsla, Option<f32>)> = parts[start_idx..]
        .iter()
        .filter_map(|part| parse_gradient_stop(part, current, dark))
        .collect();
    if stops.len() < 2 {
        return None;
    }
    let first = stops.first()?;
    let last = stops.last()?;
    Some(linear_gradient(
        angle,
        linear_color_stop(first.0, first.1.unwrap_or(0.0)),
        linear_color_stop(last.0, last.1.unwrap_or(1.0)),
    ))
}

fn parse_gradient_direction(raw: &str) -> Option<f32> {
    let s = raw.trim().to_ascii_lowercase();
    if let Some(deg) = s.strip_suffix("deg") {
        return number(deg).map(|d| d.rem_euclid(360.0));
    }
    if let Some(turn) = s.strip_suffix("turn") {
        return number(turn).map(|t| (t * 360.0).rem_euclid(360.0));
    }
    if let Some(dir) = s.strip_prefix("to ") {
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        for word in dir.split_whitespace() {
            match word {
                "top" => y = -1.0,
                "bottom" => y = 1.0,
                "left" => x = -1.0,
                "right" => x = 1.0,
                _ => return None,
            }
        }
        return Some(match (x as i32, y as i32) {
            (0, -1) => 0.0,
            (1, -1) => 45.0,
            (1, 0) => 90.0,
            (1, 1) => 135.0,
            (0, 1) => 180.0,
            (-1, 1) => 225.0,
            (-1, 0) => 270.0,
            (-1, -1) => 315.0,
            _ => return None,
        });
    }
    None
}

fn parse_gradient_stop(raw: &str, current: Hsla, dark: bool) -> Option<(Hsla, Option<f32>)> {
    let tokens = split_whitespace_preserving_parens(raw.trim());
    if tokens.is_empty() {
        return None;
    }
    let color = parse_color_for_mode(tokens[0], dark)?.resolve(current);
    let pct = tokens
        .get(1)
        .and_then(|t| t.strip_suffix('%'))
        .and_then(number)
        .map(|n| (n / 100.0).clamp(0.0, 1.0));
    Some((color, pct))
}

fn parse_box_shadows(val: &str, em: f32, current: Hsla, dark: bool) -> Vec<BoxShadow> {
    split_top_level_commas(val)
        .into_iter()
        .filter_map(|part| {
            let mut inset = false;
            let mut nums = Vec::new();
            let mut color = None;
            for tok in split_whitespace_preserving_parens(part) {
                if tok.eq_ignore_ascii_case("inset") {
                    inset = true;
                } else if let Some(p) = parse_px_f32(tok, em) {
                    nums.push(p);
                } else if let Some(c) = parse_color_for_mode(tok, dark) {
                    color = Some(c.resolve(current));
                }
            }
            if nums.len() < 2 {
                return None;
            }
            Some(BoxShadow {
                color: color.unwrap_or_else(|| Hsla::black().opacity(0.15)),
                offset: point(px(nums[0]), px(nums[1])),
                blur_radius: px(nums.get(2).copied().unwrap_or(0.0).max(0.0)),
                spread_radius: px(nums.get(3).copied().unwrap_or(0.0)),
                inset,
            })
        })
        .collect()
}

fn parse_css_string(val: &str) -> Option<String> {
    let s = val.trim();
    if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("normal") {
        return None;
    }
    if ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
        && s.len() >= 2
    {
        return Some(unescape_css_string(&s[1..s.len() - 1]));
    }
    None
}

fn unescape_css_string(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let mut hex = String::new();
            while hex.len() < 6 && chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                hex.push(chars.next().unwrap());
            }
            if !hex.is_empty() {
                if let Ok(code) = u32::from_str_radix(&hex, 16)
                    && let Some(c) = char::from_u32(code)
                {
                    out.push(c);
                }
                if chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                    chars.next();
                }
            } else if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn split_whitespace_preserving_parens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut start = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => {
                start.get_or_insert(i);
                depth += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
            }
            c if c.is_ascii_whitespace() && depth == 0 => {
                if let Some(st) = start.take() {
                    out.push(&s[st..i]);
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}
