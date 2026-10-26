//! Tcode's tab bars: styled compositions of `gpui_base::Tabs`/`Tab`.
//!
//! [`TabBar`] has three forms and one geometry per form: [`TabBar::underline`]
//! for page and view navigation, [`TabBar::segmented`] for a view switch
//! inside a panel, and [`TabBar::rail`] for a vertical navigation list. The
//! segmented drawing is shared with [`super::choice::ChoiceGroup`].
use crate::{
    icon::Icon,
    material::{self, TOUCH_TARGET, focus_ring},
    scroll::{ScrollArea, ScrollableElement as _},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Tooltip,
        menu::{ContextMenuExt as _, PopupMenu},
    },
};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, ElementId, Entity, Hsla, InteractiveElement,
    IntoElement, ParentElement, Pixels, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement as _, StyleRefinement, Styled, Window, canvas, div, point,
    prelude::FluentBuilder, px,
};
use gpui_base::motion::{Interpolate as _, Transition, transition};
use gpui_base::{StyledExt as _, Tab, Tabs};
use std::{cell::RefCell, rc::Rc};

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// The underline indicator's weight.
const INDICATOR: f32 = 2.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Underline,
    Segmented,
    Rail,
}

/// One tab of a [`TabBar`].
pub struct TabItem {
    id: ElementId,
    label: SharedString,
    icon: Option<Icon>,
    selected: bool,
    count: Option<usize>,
    trailing: Option<AnyElement>,
    on_click: Option<ClickHandler>,
    context_menu: Option<MenuBuilder>,
}

impl TabItem {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            selected: false,
            count: None,
            trailing: None,
            on_click: None,
            context_menu: None,
        }
    }
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    /// A muted count after the label, also read out with it.
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }
    /// A control or mark beside the label, such as a close button. It shares
    /// the tab's surface but not its click, focus or accessible name.
    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
    /// A right-click menu on the whole surface, including its trailing control.
    pub fn context_menu(
        mut self,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        self.context_menu = Some(Rc::new(builder));
        self
    }
}

/// The tab list of a page, panel or navigation rail.
///
/// Its own styles place the bar (width, flex, margins); the form owns
/// everything inside it.
#[derive(IntoElement)]
pub struct TabBar {
    id: ElementId,
    form: Form,
    compact: bool,
    icon_only: bool,
    hairline: bool,
    label_max_w: Option<Pixels>,
    aria_label: Option<SharedString>,
    items: Vec<TabItem>,
    style: StyleRefinement,
}

impl TabBar {
    /// An underline bar, the default form.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            form: Form::Underline,
            compact: false,
            icon_only: false,
            hairline: true,
            label_max_w: None,
            aria_label: None,
            items: Vec::new(),
            style: StyleRefinement::default(),
        }
    }
    /// Horizontal page or view navigation over a hairline.
    pub fn underline(mut self) -> Self {
        self.form = Form::Underline;
        self
    }
    /// Whether the underline bar draws its separator. Disable when its toolbar
    /// draws the same hairline so translucent borders are painted only once.
    pub fn hairline(mut self, hairline: bool) -> Self {
        self.hairline = hairline;
        self
    }
    /// A horizontal view switch inside a panel: a pill track.
    pub fn segmented(mut self) -> Self {
        self.form = Form::Segmented;
        self
    }
    /// A vertical navigation list.
    pub fn rail(mut self) -> Self {
        self.form = Form::Rail;
        self
    }
    /// Touch geometry: every tab is at least a [`TOUCH_TARGET`] tall.
    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }
    /// Tabs show only their icons, each named by a tooltip with its label.
    pub fn icon_only(mut self, icon_only: bool) -> Self {
        self.icon_only = icon_only;
        self
    }
    /// Clamp visible labels while keeping their full accessible names.
    /// Unset by default; overflowing bars still scroll.
    pub fn label_max_w(mut self, width: impl Into<Pixels>) -> Self {
        self.label_max_w = Some(width.into());
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    pub fn item(mut self, item: TabItem) -> Self {
        self.items.push(item);
        self
    }
    pub fn items(mut self, items: impl IntoIterator<Item = TabItem>) -> Self {
        self.items.extend(items);
        self
    }
}

impl Styled for TabBar {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

/// Per-form sizes. Desktop tabs share the 28pt of a small toolbar control;
/// compact tabs are a full touch target. A rail row's height, padding and
/// gap are [`material::selectable_row`]'s.
struct Metrics {
    /// The height of each tab's surface.
    item: f32,
    /// The surface's inline padding.
    pad: f32,
    /// The bar's height, for horizontal forms.
    bar: f32,
    /// An icon-only tab's glyph; beside a label the icon takes the text size.
    icon: f32,
}

fn metrics(form: Form, compact: bool) -> Metrics {
    match (form, compact) {
        (Form::Underline, false) => Metrics {
            item: 28.,
            pad: 10.,
            bar: 40.,
            icon: 16.,
        },
        (Form::Underline, true) => Metrics {
            item: TOUCH_TARGET,
            pad: 12.,
            bar: TOUCH_TARGET,
            icon: 18.,
        },
        (Form::Segmented, compact) => {
            let item = segment_height(compact);
            Metrics {
                item,
                pad: if compact { 14. } else { 12. },
                bar: item + 2. * TRACK_INSET,
                icon: if compact { 18. } else { 16. },
            }
        }
        (Form::Rail, compact) => Metrics {
            item: material::selectable_row_height(compact),
            pad: 0.,
            bar: 0.,
            icon: if compact { 20. } else { 16. },
        },
    }
}

fn segment_height(compact: bool) -> f32 {
    if compact { TOUCH_TARGET } else { 28. }
}

/// Where the underline mark starts and ends inside its tab: inside the tab's
/// padding, or under the label alone when a trailing control follows it.
fn underline_inset(m: &Metrics, compact: bool, icon_only: bool, trailing: bool) -> (f32, f32) {
    if icon_only {
        (6., 6.)
    } else if trailing {
        (m.pad, if compact { 8. } else { 4. })
    } else {
        (m.pad, m.pad)
    }
}

/// A selection mark (the underline, the segment pill) drawn once for the
/// whole bar, so it can glide from one item to the next.
///
/// The bar learns where its items are only after they are laid out, so each
/// frame measures them and the next frame draws from that measurement. Until
/// every item has been measured the selected item draws its own mark.
pub(crate) struct Glide {
    /// Item bounds relative to the list, from the last frame.
    geometry: Entity<Vec<Bounds<Pixels>>>,
    /// This frame's measurements, in window coordinates.
    measured: Rc<RefCell<Vec<Option<Bounds<Pixels>>>>>,
    /// The selected index, gliding.
    at: Option<f32>,
    drawn: bool,
}

impl Glide {
    pub(crate) fn new(
        id: &ElementId,
        count: usize,
        selected: Option<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let geometry =
            window.use_keyed_state((id.clone(), "glide-geometry"), cx, |_, _| Vec::new());
        let at = selected.map(|ix| {
            transition(
                (id.clone(), "glide"),
                ix as f32,
                Transition::new(crate::edge_back::SETTLE),
                window,
                cx,
            )
        });
        let drawn = at.is_some() && count > 0 && geometry.read(cx).len() == count;
        Self {
            geometry,
            measured: Rc::new(RefCell::new(vec![None; count])),
            at,
            drawn,
        }
    }

    /// Whether the bar draws the mark this frame.
    pub(crate) fn drawn(&self) -> bool {
        self.drawn
    }

    /// The mark's bounds relative to the list: `mark` places it inside the
    /// item at an index, and the result lies between the two items the
    /// gliding index falls between.
    fn span(
        &self,
        cx: &App,
        mark: impl Fn(usize, Bounds<Pixels>) -> Bounds<Pixels>,
    ) -> Option<Bounds<Pixels>> {
        let at = self.at.filter(|_| self.drawn)?;
        let items = self.geometry.read(cx);
        let at = at.clamp(0., items.len().checked_sub(1)? as f32);
        let (lo, hi) = (at.floor() as usize, at.ceil() as usize);
        Some(mark(lo, items[lo]).interpolate(&mark(hi, items[hi]), at - lo as f32))
    }

    /// Records where item `ix` lands this frame. `el` must be positioned.
    pub(crate) fn measure<E: ParentElement>(&self, ix: usize, el: E) -> E {
        let measured = self.measured.clone();
        el.child(pinned_canvas(move |bounds| {
            if let Some(slot) = measured.borrow_mut().get_mut(ix) {
                *slot = Some(bounds);
            }
        }))
    }

    /// Turns this frame's measurements into the next frame's geometry. Goes
    /// on the list holding the items, after them, so it runs once they have
    /// all been measured. `list` must be positioned.
    pub(crate) fn observe<E: ParentElement>(&self, list: E) -> E {
        let (measured, geometry) = (self.measured.clone(), self.geometry.clone());
        list.child(pinned_canvas_cx(move |list, cx| {
            let items: Option<Vec<_>> = measured
                .borrow()
                .iter()
                .map(|item| item.map(|item| Bounds::new(item.origin - list.origin, item.size)))
                .collect();
            let Some(items) = items else { return };
            geometry.update(cx, |geometry, cx| {
                if *geometry != items {
                    *geometry = items;
                    cx.notify();
                }
            });
        }))
    }

    /// The segment pill under the selected segment, for the list's first
    /// child so the segments paint over it. `dim` matches a disabled
    /// selected segment.
    pub(crate) fn pill(&self, dim: bool, cx: &App) -> Option<AnyElement> {
        let span = self.span(cx, |_, item| item)?;
        let pill = div()
            .absolute()
            .left(span.left())
            .top(span.top())
            .w(span.size.width)
            .h(span.size.height)
            .rounded(segment_radius(cx))
            .bg(raised_segment(cx))
            .shadow_xs()
            .when(dim, |this| this.opacity(0.5));
        Some(pill.into_any_element())
    }
}

/// A canvas covering its parent's padding box that reports the parent's
/// bounds at prepaint. `gpui_base::ElementExt::on_prepaint` leaves its canvas
/// at the static position, which a padded parent offsets by its padding.
fn pinned_canvas(on_bounds: impl FnOnce(Bounds<Pixels>) + 'static) -> impl IntoElement {
    pinned_canvas_cx(move |bounds, _| on_bounds(bounds))
}

fn pinned_canvas_cx(
    on_bounds: impl FnOnce(Bounds<Pixels>, &mut App) + 'static,
) -> impl IntoElement {
    canvas(move |bounds, _, cx| on_bounds(bounds, cx), |_, _, _, _| {})
        .absolute()
        .top_0()
        .left_0()
        .size_full()
}

/// A selected label's colour, gliding with the selection.
pub(crate) fn label_color(
    id: &ElementId,
    channel: &'static str,
    target: Hsla,
    window: &mut Window,
    cx: &mut App,
) -> Hsla {
    transition(
        (id.clone(), channel),
        target,
        Transition::new(crate::edge_back::SETTLE),
        window,
        cx,
    )
}

impl RenderOnce for TabBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            form,
            compact,
            icon_only,
            hairline,
            label_max_w,
            aria_label,
            items,
            style,
        } = self;
        let m = metrics(form, compact);
        let count = items.len();
        let selected = items.iter().position(|item| item.selected);
        let insets: Vec<_> = items
            .iter()
            .map(|item| underline_inset(&m, compact, icon_only, item.trailing.is_some()))
            .collect();
        let glide = (form != Form::Rail).then(|| Glide::new(&id, count, selected, window, cx));
        let tabs: Vec<_> = items
            .into_iter()
            .enumerate()
            .map(|(ix, item)| {
                render_item(
                    item,
                    ix,
                    count,
                    form,
                    compact,
                    icon_only,
                    label_max_w,
                    &m,
                    glide.as_ref(),
                    window,
                    cx,
                )
            })
            .collect();
        let pill = glide
            .as_ref()
            .filter(|_| form == Form::Segmented)
            .and_then(|glide| glide.pill(false, cx));
        let underline = glide
            .as_ref()
            .filter(|_| form == Form::Underline)
            .and_then(|glide| {
                glide.span(cx, |ix, tab| {
                    let (start, end) = insets[ix];
                    Bounds::from_corners(
                        point(tab.left() + px(start), tab.top()),
                        point(tab.right() - px(end), tab.bottom()),
                    )
                })
            })
            .map(|span| {
                // Bottom-aligned with the bar's hairline, which it covers.
                div()
                    .absolute()
                    .left(span.left())
                    .w(span.size.width)
                    .bottom_0()
                    .h(px(INDICATOR))
                    .rounded(px(INDICATOR / 2.))
                    .bg(cx.theme().foreground)
            });
        let list = Tabs::new(id.clone())
            .when_some(aria_label, |this, label| this.aria_label(label))
            .children(pill)
            .children(tabs)
            .children(underline)
            .when_some(glide.as_ref(), |list, glide| glide.observe(list.relative()));
        match form {
            Form::Underline => {
                let list = list
                    .flex()
                    .items_center()
                    .h_full()
                    .min_w_full()
                    .flex_shrink_0()
                    .gap_1();
                let viewport = div()
                    .id((id.clone(), "viewport"))
                    .flex()
                    .h(px(m.bar))
                    .overflow_x_scroll_area()
                    .child(list);
                div()
                    .debug_selector(|| id.to_string())
                    .relative()
                    .min_w_0()
                    .max_w_full()
                    .h(px(m.bar))
                    .min_h(px(m.bar))
                    .when(hairline, |bar| {
                        bar.child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(1.))
                                .bg(cx.theme().border),
                        )
                    })
                    .child(viewport)
                    .refine_style(&style)
                    .into_any_element()
            }
            Form::Segmented => segmented_track(id.clone(), list, compact, cx)
                .debug_selector(|| id.to_string())
                .refine_style(&style)
                .into_any_element(),
            Form::Rail => list
                .debug_selector(|| id.to_string())
                .flex()
                .flex_col()
                .when(icon_only, |this| this.items_center())
                .gap(px(2.))
                .refine_style(&style)
                .into_any_element(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_item(
    item: TabItem,
    ix: usize,
    count: usize,
    form: Form,
    compact: bool,
    icon_only: bool,
    label_max_w: Option<Pixels>,
    m: &Metrics,
    glide: Option<&Glide>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let TabItem {
        id,
        label,
        icon,
        selected,
        count: tally,
        trailing,
        on_click,
        context_menu,
    } = item;
    // A rail reads as a list: its labels are body text, its icons muted.
    let (fg, icon_fg) = {
        let theme = cx.theme();
        match form {
            _ if selected => (theme.foreground, theme.foreground),
            Form::Rail => (theme.foreground, theme.muted_foreground),
            _ => (theme.muted_foreground, theme.muted_foreground),
        }
    };
    let fg = label_color(&id, "fg", fg, window, cx);
    let icon_fg = label_color(&id, "icon-fg", icon_fg, window, cx);
    // The selected item draws its own mark only until the bar can.
    let mark = selected && !glide.is_some_and(Glide::drawn);
    let theme = cx.theme();
    let ring = focus_ring(cx);
    let accessible = match tally {
        Some(tally) => crate::tr!("tabs.with_count", label = label.clone(), count = tally)
            .into_owned()
            .into(),
        None => label.clone(),
    };
    let trailing = trailing.filter(|_| !icon_only);
    let gap = px(if compact { 8. } else { 6. });
    // The item id doubles as its debug selector, so a test finds a tab by the
    // id its call site gave it.
    let selector = id.to_string();
    let tab = Tab::new(id.clone())
        .debug_selector(move || selector)
        .selected(selected)
        .set_position(ix + 1, count)
        .accessibility_label(accessible)
        .tab_index(0)
        .focus_visible(move |style| style.shadow(vec![ring]))
        .cursor_pointer()
        .gap(gap)
        .when_some(icon, |this, icon| {
            let color = icon.explicit_text_color().unwrap_or(icon_fg);
            let icon = if icon_only {
                icon.size(px(m.icon))
            } else {
                icon
            };
            this.child(icon.flex_none().text_color(color))
        })
        .when(!icon_only, |this| {
            this.child(
                div()
                    .flex_none()
                    .when_some(label_max_w, |label, width| {
                        label.max_w(width).overflow_hidden()
                    })
                    .child(super::fade_text(label.clone())),
            )
            .when_some(tally, |this, tally| {
                this.child(
                    div()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(tally.to_string()),
                )
            })
        })
        .when(icon_only, |this| {
            this.tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
        })
        .when_some(on_click, |tab, handler| {
            tab.on_click(move |event, window, cx| handler(event, window, cx))
        });
    let Some(trailing) = trailing else {
        let tab = tab.text_color(fg);
        let tab = surface(tab, form, selected, mark, compact, icon_only, false, m, cx);
        let tab = match glide {
            Some(glide) => glide.measure(ix, tab),
            None => tab,
        };
        return match context_menu {
            Some(builder) => tab
                .context_menu(move |menu, window, cx| builder(menu, window, cx))
                .into_any_element(),
            None => tab.into_any_element(),
        };
    };
    // The trailing control shares the tab's surface but sits beside the tab,
    // outside its click, focus and accessible name.
    let row = div()
        .id((id, "surface"))
        .text_color(fg)
        .child(
            tab.h_full()
                .rounded(surface_radius(form, cx))
                .when(form == Form::Rail, |tab| {
                    tab.flex_1().min_w_0().justify_start()
                }),
        )
        .child(div().flex_none().flex().items_center().child(trailing));
    let row = surface(row, form, selected, mark, compact, icon_only, true, m, cx);
    let row = match glide {
        Some(glide) => glide.measure(ix, row),
        None => row,
    };
    match context_menu {
        Some(builder) => row
            .context_menu(move |menu, window, cx| builder(menu, window, cx))
            .into_any_element(),
        None => row.into_any_element(),
    }
}

/// The visible surface of one tab: the tab itself, or the row holding the tab
/// and its trailing control. `mark` draws the selection mark on it, for a bar
/// that cannot yet draw its gliding one.
#[allow(clippy::too_many_arguments)]
fn surface<E: Styled + InteractiveElement + ParentElement + FluentBuilder>(
    el: E,
    form: Form,
    selected: bool,
    mark: bool,
    compact: bool,
    icon_only: bool,
    trailing: bool,
    m: &Metrics,
    cx: &App,
) -> E {
    let theme = cx.theme();
    let foreground = theme.foreground;
    let (start, end) = underline_inset(m, compact, icon_only, trailing);
    let el = el
        .relative()
        .flex()
        .items_center()
        .flex_none()
        .text_body(compact, cx);
    match form {
        // Only a rail may change weight with selection: its labels do not
        // share a line.
        Form::Rail => material::selectable_row(el, selected, compact, cx)
            .when(selected, |this| this.font_medium())
            .when(icon_only, |this| this.w(px(m.item)).px_0().justify_center())
            .when(!icon_only, |this| this.w_full().justify_start()),
        // Weight does not change with selection on a horizontal bar: a bolder
        // selected label would widen its tab and shift its neighbours.
        Form::Underline | Form::Segmented => el
            .font_medium()
            .gap(px(if compact { 8. } else { 6. }))
            .map(|this| {
                if icon_only {
                    this.min_w(px(m.item)).justify_center()
                } else {
                    this.pl(px(start)).pr(px(end))
                }
            })
            .map(|this| {
                if form == Form::Segmented {
                    return segment(this, selected, mark, true, compact, cx);
                }
                this.h(px(m.item))
                    .rounded(surface_radius(form, cx))
                    .when(!selected, |this| {
                        this.hover(move |style| style.bg(theme.muted).text_color(foreground))
                    })
                    .when(mark, |this| {
                        // Bottom-aligned with the bar's hairline, which it covers.
                        this.child(
                            div()
                                .absolute()
                                .left(px(start))
                                .right(px(end))
                                .bottom(px(-(m.bar - m.item) / 2.))
                                .h(px(INDICATOR))
                                .rounded(px(INDICATOR / 2.))
                                .bg(foreground),
                        )
                    })
            }),
    }
}

fn surface_radius(form: Form, cx: &App) -> Pixels {
    match form {
        Form::Segmented => segment_radius(cx),
        Form::Underline | Form::Rail => material::radius_button(cx),
    }
}

/// The track's padding around its segments.
const TRACK_INSET: f32 = 2.;

/// A segment's corners sit concentric inside the track's.
fn segment_radius(cx: &App) -> Pixels {
    material::radius_input(cx) - px(TRACK_INSET)
}

/// The pill track shared by [`TabBar::segmented`] and
/// [`ChoiceGroup`](super::choice::ChoiceGroup): `list` holds the segments and
/// scrolls sideways once they no longer fit, and otherwise its segments divide
/// the width the track is given.
pub(crate) fn segmented_track(
    id: impl Into<ElementId>,
    list: impl Styled + ParentElement + IntoElement,
    compact: bool,
    cx: &App,
) -> ScrollArea<Stateful<Div>> {
    let id = id.into();
    let height = segment_height(compact) + 2. * TRACK_INSET;
    div()
        .id((id, "viewport"))
        .flex()
        .h(px(height))
        .min_h(px(height))
        .min_w_0()
        .overflow_x_scroll_area()
        .child(
            list.flex()
                .items_center()
                .h_full()
                .min_w_full()
                .flex_shrink_0()
                .gap(px(2.))
                .p(px(TRACK_INSET))
                .rounded(material::radius_input(cx))
                .bg(cx.theme().secondary),
        )
}

/// The selected segment's pill.
fn raised_segment(cx: &App) -> Hsla {
    let theme = cx.theme();
    // The popover surface is the lightest plane in the light theme but
    // matches the track in the dark one, where the pill is lifted with the
    // foreground instead.
    if theme.mode.is_dark() {
        theme.foreground.opacity(0.12)
    } else {
        theme.popover
    }
}

/// One segment's surface: transparent at rest. `pill` raises it itself, for
/// a track that cannot yet draw its gliding [`Glide::pill`]. `interactive` is
/// false for a segment that cannot be chosen.
pub(crate) fn segment<E: Styled + InteractiveElement + FluentBuilder>(
    el: E,
    selected: bool,
    pill: bool,
    interactive: bool,
    compact: bool,
    cx: &App,
) -> E {
    let raised = raised_segment(cx);
    let theme = cx.theme();
    let (muted, foreground) = (theme.muted, theme.foreground);
    el.relative()
        .h(px(segment_height(compact)))
        .flex_grow(1.)
        .flex_basis(px(0.))
        .flex_shrink_0()
        .justify_center()
        .rounded(segment_radius(cx))
        .when(pill, |this| this.bg(raised).shadow_xs())
        .when(!selected && interactive, |this| {
            this.hover(move |style| style.bg(muted).text_color(foreground))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Modifiers, Render, TestAppContext, VisualTestContext};
    use std::cell::Cell;

    struct Segments(Vec<&'static str>);

    impl Render for Segments {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(320.)).child(
                TabBar::new("track").segmented().items(
                    self.0
                        .iter()
                        .enumerate()
                        .map(|(ix, label)| TabItem::new(("segment", ix), *label).selected(ix == 0)),
                ),
            )
        }
    }

    fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<Pixels> {
        cx.debug_bounds(selector).unwrap()
    }

    #[gpui::test]
    fn short_segments_divide_the_track_and_long_ones_overflow_it(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| Segments(vec!["One", "Two", "Three"]));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let track = bounds(cx, "track");
        let segments = ["segment-0", "segment-1", "segment-2"].map(|s| bounds(cx, s));
        assert_eq!(track.size.width, px(320.));
        assert_eq!(segments[0].left(), track.left() + px(TRACK_INSET));
        assert_eq!(segments[2].right(), track.right() - px(TRACK_INSET));
        assert!(
            segments
                .windows(2)
                .all(|pair| pair[1].left() == pair[0].right() + px(2.)),
            "{segments:?}"
        );

        let long = "A label too long to share a phone-width track";
        let (_, cx) = cx.add_window_view(move |_, _| Segments(vec![long, long, long]));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let track = bounds(cx, "track");
        let last = bounds(cx, "segment-2");
        assert_eq!(track.size.width, px(320.));
        assert!(last.right() > track.right(), "{last:?}");
    }

    struct Closable(Rc<Cell<(usize, usize)>>);

    impl Render for Closable {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let (select, close) = (self.0.clone(), self.0.clone());
            TabBar::new("bar").item(
                TabItem::new("tab", "Terminal")
                    .on_click(move |_, _, _| select.set((select.get().0 + 1, select.get().1)))
                    .trailing(
                        crate::widgets::Button::new("close")
                            .debug_selector(|| "close".into())
                            .label("x")
                            .on_click(move |_, _, _| close.set((close.get().0, close.get().1 + 1))),
                    ),
            )
        }
    }

    #[gpui::test]
    fn a_trailing_control_does_not_activate_its_tab(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let clicks = Rc::new(Cell::new((0, 0)));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Closable(clicks)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let close = bounds(cx, "close");
        cx.simulate_click(close.center(), Modifiers::default());
        assert_eq!(clicks.get(), (0, 1));
        let tab = bounds(cx, "tab");
        cx.simulate_click(tab.center(), Modifiers::default());
        assert_eq!(clicks.get(), (1, 1));
    }
}
