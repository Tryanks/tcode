//! Tcode's tab bars: styled compositions of `gpui_base::Tabs`/`Tab`.
//!
//! [`TabBar`] has three forms and one geometry per form: [`TabBar::underline`]
//! for page and view navigation, [`TabBar::segmented`] for a view switch
//! inside a panel, and [`TabBar::rail`] for a vertical navigation list. The
//! segmented drawing is shared with [`super::choice::ChoiceGroup`].
use crate::widgets::TooltipTrigger as _;
use crate::{
    icon::Icon,
    material::{self, TOUCH_TARGET},
    motion::keyed::{Fold, keyed_in},
    scroll::{ScrollArea, ScrollableElement as _},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Tooltip,
        menu::{ContextMenuExt as _, PopupMenu},
    },
};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, ElementId, Hsla, InteractiveElement,
    IntoElement, ParentElement, Pixels, Refineable as _, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div, point,
    prelude::FluentBuilder, px,
};
use gpui_base::motion::{Transition, transition};
use gpui_base::{ElementExt as _, StyledExt as _, Tab, Tabs};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;
type ElementBuilder = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// The underline indicator's weight.
const INDICATOR: f32 = 2.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Underline,
    Segmented,
    Rail,
}

/// One tab of a [`TabBar`].
#[derive(Clone)]
pub struct TabItem {
    id: ElementId,
    label: SharedString,
    icon: Option<Icon>,
    selected: bool,
    count: Option<usize>,
    trailing: Option<ElementBuilder>,
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
    /// the tab's surface but not its click, focus or accessible name. It is
    /// built each time the tab is drawn, so a tab that goes is drawn with it
    /// while it folds away.
    pub fn trailing(
        mut self,
        build: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        self.trailing = Some(Rc::new(build));
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
/// everything inside it. A tab that joins the bar folds in, and one that
/// leaves it folds away, as the others move over and the selection mark
/// follows its tab.
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
    /// A rail's mark kept by the column that holds several rails.
    mark: Option<Rc<Glide>>,
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
            mark: None,
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
    /// A rail that is one of several in a column sharing one selection:
    /// its items are placed in `mark`, which the column draws with
    /// [`Glide::rail`] under all of them, so the selected fill glides from
    /// one rail to the next as it does within one.
    pub(crate) fn shared_mark(mut self, mark: Rc<Glide>) -> Self {
        self.mark = Some(mark);
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

type Places = Rc<RefCell<HashMap<SharedString, Bounds<Pixels>>>>;
/// Where a mark lies inside an item, given the item's bounds and its list's.
type Place = Box<dyn Fn(&SharedString, Bounds<Pixels>, Bounds<Pixels>) -> Bounds<Pixels>>;

/// A selection mark (the underline, the segment pill) drawn once for the
/// whole bar, so it can glide from one item to the next, and follow its
/// item while other items fold in and out around it.
///
/// Each item records where it is placed this frame, and the mark, painted
/// once every item has been placed, is drawn from those places in the same
/// frame: between the items the selection is leaving, by how far it still
/// is from them, and the item it is going to.
pub(crate) struct Glide {
    /// Where each item is placed this frame, by key, in window coordinates.
    places: Places,
    /// The items the mark lies between, and how much of the mark each holds.
    weights: Vec<(SharedString, f32)>,
}

/// Where a bar's selection mark is going and what it is leaving.
#[derive(Default)]
struct Selection {
    /// The items the mark lay between when the selection last changed.
    from: Vec<(SharedString, f32)>,
    to: Option<SharedString>,
    /// Counts the changes, naming each one's progress.
    change: u64,
}

impl Glide {
    /// The mark of the bar `id` with the item `selected` selected, by key.
    pub(crate) fn new(
        id: &ElementId,
        selected: Option<SharedString>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let state = window.use_keyed_state((id.clone(), "glide"), cx, |_, _| Selection::default());
        let progress_id = |change: u64| (id.clone(), SharedString::from(format!("glide-{change}")));
        let settle = || Transition::new(GLIDE);
        let (to, change) = {
            let state = state.read(cx);
            (state.to.clone(), state.change)
        };
        if to != selected {
            let from = match (&to, &selected) {
                // From wherever the mark is now, part way or at rest.
                (Some(_), Some(_)) => {
                    let progress = transition(progress_id(change), 1f32, settle(), window, cx);
                    let from = blend(&state.read(cx).from, to.as_ref(), progress);
                    // Starts from nothing rather than at rest.
                    transition(progress_id(change + 1), 0f32, settle(), window, cx);
                    from
                }
                // A mark that appears or goes does so at once.
                _ => Vec::new(),
            };
            state.update(cx, |state, _| {
                state.from = from;
                state.to = selected;
                state.change += 1;
            });
        }
        let (weights, at_rest) = {
            let change = state.read(cx).change;
            let progress = transition(progress_id(change), 1f32, settle(), window, cx);
            let state = state.read(cx);
            (
                blend(&state.from, state.to.as_ref(), progress),
                progress >= 1. && !state.from.is_empty(),
            )
        };
        if at_rest {
            state.update(cx, |state, _| state.from.clear());
        }
        Self {
            places: Places::default(),
            weights,
        }
    }

    /// Records where item `key` is placed this frame.
    pub(crate) fn measure<E: ParentElement>(&self, key: SharedString, el: E) -> E {
        let places = self.places.clone();
        el.on_prepaint(move |bounds, _, _| {
            places.borrow_mut().insert(key.clone(), bounds);
        })
    }

    /// The mark, for the list holding the items, which must be positioned:
    /// `place` puts it inside an item, given the item's bounds and the
    /// list's, and `style` draws it.
    fn mark(
        &self,
        place: impl Fn(&SharedString, Bounds<Pixels>, Bounds<Pixels>) -> Bounds<Pixels> + 'static,
        style: StyleRefinement,
    ) -> AnyElement {
        Mark {
            places: self.places.clone(),
            weights: self.weights.clone(),
            place: Box::new(place),
            style,
        }
        .into_any_element()
    }

    /// The segment pill under the selected segment, for the list's first
    /// child so the segments paint over it. `dim` matches a disabled
    /// selected segment.
    pub(crate) fn pill(&self, dim: bool, cx: &App) -> AnyElement {
        let raised = raised_segment(cx);
        let mut pill = div()
            .rounded(segment_radius(cx))
            .bg(if dim { raised.opacity(0.5) } else { raised })
            .shadow_xs();
        let mut style = pill.style().clone();
        if dim {
            for shadow in style.box_shadow.iter_mut().flatten() {
                shadow.color = shadow.color.opacity(0.5);
            }
        }
        self.mark(|_, item, _| item, style)
    }

    /// A rail's selected fill under the selected row, for the first child
    /// of the positioned element holding the rail (or rails) whose items
    /// are placed in this glide, so the rows paint over it.
    pub(crate) fn rail(&self, cx: &App) -> AnyElement {
        let style = div()
            .rounded(cx.theme().tokens.radius.md)
            .bg(cx.theme().list_active)
            .style()
            .clone();
        self.mark(|_, item, _| item, style)
    }
}

/// The weights of `from`, scaled by how far the selection is from them, with
/// `to` holding the rest.
fn blend(
    from: &[(SharedString, f32)],
    to: Option<&SharedString>,
    progress: f32,
) -> Vec<(SharedString, f32)> {
    let progress = if from.is_empty() {
        1.
    } else {
        progress.clamp(0., 1.)
    };
    let mut weights: Vec<(SharedString, f32)> = from
        .iter()
        .map(|(key, weight)| (key.clone(), weight * (1. - progress)))
        .collect();
    if let Some(to) = to {
        match weights.iter_mut().find(|(key, _)| key == to) {
            Some((_, weight)) => *weight += progress,
            None => weights.push((to.clone(), progress)),
        }
    }
    weights.retain(|(_, weight)| *weight > 0.);
    weights
}

/// A selection mark, painted from where its items were placed this frame.
struct Mark {
    places: Places,
    weights: Vec<(SharedString, f32)>,
    place: Place,
    style: StyleRefinement,
}

impl IntoElement for Mark {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for Mark {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        // Over the whole list, taking no place in it.
        let style = gpui::Style {
            position: gpui::Position::Absolute,
            inset: gpui::Edges::all(px(0.).into()),
            ..Default::default()
        };
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        list: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let places = self.places.borrow();
        let (mut total, mut edges) = (0f32, [0f32; 4]);
        for (key, weight) in &self.weights {
            let Some(item) = places.get(key) else {
                continue;
            };
            let mark = (self.place)(key, *item, list);
            for (edge, value) in
                edges
                    .iter_mut()
                    .zip([mark.left(), mark.top(), mark.right(), mark.bottom()])
            {
                *edge += value.as_f32() * weight;
            }
            total += weight;
        }
        if total <= 0. {
            return;
        }
        let [left, top, right, bottom] = edges.map(|edge| px(edge / total));
        let mut style = gpui::Style::default();
        style.refine(&self.style);
        style.paint(
            Bounds::from_corners(point(left, top), point(right, bottom)),
            window,
            cx,
            |_, _| {},
        );
    }
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
        Transition::new(GLIDE),
        window,
        cx,
    )
}

/// The key a tab keeps while it comes, stays and goes: its id.
pub(crate) fn item_key(id: &ElementId) -> SharedString {
    id.to_string().into()
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
            mark,
            style,
        } = self;
        let m = metrics(form, compact);
        let count = items.len();
        let keyed: Vec<_> = items
            .into_iter()
            .map(|item| (item_key(&item.id), item))
            .collect();
        let positions: HashMap<SharedString, usize> = keyed
            .iter()
            .enumerate()
            .map(|(ix, (key, _))| (key.clone(), ix))
            .collect();
        let selected = keyed
            .iter()
            .find(|(_, item)| item.selected)
            .map(|(key, _)| key.clone());
        let shared = mark.is_some();
        let glide = mark.unwrap_or_else(|| Rc::new(Glide::new(&id, selected, window, cx)));
        let insets: Rc<RefCell<HashMap<SharedString, (f32, f32)>>> = Rc::default();
        let fold = match form {
            Form::Underline => Fold::Across(px(UNDERLINE_GAP)),
            Form::Segmented => Fold::Share(px(SEGMENT_GAP)),
            Form::Rail => Fold::Down,
        };
        let tabs = keyed_in(
            (id.clone(), "tabs"),
            fold,
            keyed,
            ROWS,
            window,
            cx,
            |key, item, window, cx| {
                let position = positions.get(key).map(|ix| (*ix, count));
                let mut item = item.clone();
                // A tab on its way out is no longer the selected one.
                item.selected &= position.is_some();
                insets.borrow_mut().insert(
                    key.clone(),
                    underline_inset(&m, compact, icon_only, item.trailing.is_some()),
                );
                render_item(
                    key,
                    item,
                    position,
                    form,
                    compact,
                    icon_only,
                    label_max_w,
                    &m,
                    &glide,
                    window,
                    cx,
                )
            },
        );
        let rail = (form == Form::Rail && !shared).then(|| glide.rail(cx));
        let pill = (form == Form::Segmented).then(|| glide.pill(false, cx));
        let underline = (form == Form::Underline).then(|| {
            let insets = insets.clone();
            let pad = m.pad;
            glide.mark(
                move |key, tab, list| {
                    let (start, end) = insets.borrow().get(key).copied().unwrap_or((pad, pad));
                    // Bottom-aligned with the bar's hairline, which it covers.
                    Bounds::from_corners(
                        point(tab.left() + px(start), list.bottom() - px(INDICATOR)),
                        point(tab.right() - px(end), list.bottom()),
                    )
                },
                div()
                    .rounded(px(INDICATOR / 2.))
                    .bg(cx.theme().foreground)
                    .style()
                    .clone(),
            )
        });
        let list = Tabs::new(id.clone())
            .when_some(aria_label, |this, label| this.aria_label(label))
            .relative()
            .children(pill)
            .children(rail);
        match form {
            Form::Underline => {
                let list = list
                    .flex()
                    .items_center()
                    .h_full()
                    .min_w_full()
                    .flex_shrink_0()
                    .gap(px(UNDERLINE_GAP));
                let viewport = div()
                    .id((id.clone(), "viewport"))
                    .flex()
                    .h(px(m.bar))
                    .overflow_x_scroll_area()
                    .child(tabs.followed_by(underline).render(list));
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
            Form::Segmented => {
                let list = tabs.render(segment_list(list, cx));
                segmented_track(id.clone(), list, compact)
                    .debug_selector(|| id.to_string())
                    .refine_style(&style)
                    .into_any_element()
            }
            Form::Rail => {
                let list = list
                    .debug_selector(|| id.to_string())
                    .flex()
                    .flex_col()
                    .when(icon_only, |this| this.items_center())
                    .gap(px(2.))
                    .refine_style(&style);
                tabs.render(list)
            }
        }
    }
}

/// The space between an underline bar's tabs.
const UNDERLINE_GAP: f32 = 4.;

#[allow(clippy::too_many_arguments)]
fn render_item(
    key: &SharedString,
    item: TabItem,
    position: Option<(usize, usize)>,
    form: Form,
    compact: bool,
    icon_only: bool,
    label_max_w: Option<Pixels>,
    m: &Metrics,
    glide: &Glide,
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
    // A rail's rows glide their own hover fill; a bar's tab lifts its label
    // and fills under the pointer, both gliding.
    let (hovered, on_hover) = crate::motion::hovered((id.clone(), "hover"), window, cx);
    let lit = hovered && !selected && form != Form::Rail;
    let fg = label_color(
        &id,
        "fg",
        if lit { cx.theme().foreground } else { fg },
        window,
        cx,
    );
    let icon_fg = label_color(&id, "icon-fg", icon_fg, window, cx);
    let muted = cx.theme().muted;
    let hover_fill = label_color(
        &id,
        "hover-fill",
        if lit { muted } else { muted.opacity(0.) },
        window,
        cx,
    );
    let on_hover = Rc::new(on_hover);
    let trailing = trailing
        .filter(|_| !icon_only)
        .map(|build| build(window, cx));
    // gpui-base's Tab takes no focus of its own; a `tab_index` tab stop's
    // handle is GPUI's, leaving only `focus_visible`, which switches at once,
    // to show the keyboard's focus. The tab tracks this handle instead, and
    // the ring fades in and out over it (`motion::focus_ring`).
    let focus = crate::motion::focus_ring::handle(&id, window, cx);
    let theme = cx.theme();
    let accessible = match tally {
        Some(tally) => crate::tr!("tabs.with_count", label = label.clone(), count = tally)
            .into_owned()
            .into(),
        None => label.clone(),
    };
    let gap = px(if compact { 8. } else { 6. });
    // The item id doubles as its debug selector, so a test finds a tab by the
    // id its call site gave it.
    let selector = id.to_string();
    let tab = Tab::new(id.clone())
        .debug_selector(move || selector)
        .selected(selected)
        .when_some(position, |tab, (ix, count)| tab.set_position(ix + 1, count))
        .accessibility_label(accessible)
        .track_focus(&focus)
        .cursor_pointer()
        .gap(gap);
    let has_trailing = trailing.is_some();
    // The surface goes on first, so its fill lies under the content.
    let tab = if has_trailing {
        tab
    } else {
        surface(
            tab.text_color(fg),
            form,
            selected,
            hover_fill,
            compact,
            icon_only,
            false,
            m,
            cx,
        )
        .when(form != Form::Rail, |tab| {
            let on_hover = on_hover.clone();
            tab.on_hover(move |hovered, window, cx| on_hover(hovered, window, cx))
        })
    };
    let tab = tab
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
            this.with_tooltip(move |_, _| Tooltip::new(label.clone()))
        })
        .when_some(on_click, |tab, handler| {
            tab.on_click(move |event, window, cx| handler(event, window, cx))
        });
    let Some(trailing) = trailing else {
        let mut tab = glide.measure(key.clone(), tab);
        let corners = tab.style().corner_radii.clone();
        let tab = match context_menu {
            Some(builder) => tab
                .context_menu(move |menu, window, cx| builder(menu, window, cx))
                .into_any_element(),
            None => tab.into_any_element(),
        };
        return crate::motion::focus_ring::Ringed::new(tab, &focus, corners, cx).into_any_element();
    };
    // The trailing control shares the tab's surface but sits beside the tab,
    // outside its click, focus and accessible name.
    let row = surface(
        div().id((id, "surface")).text_color(fg),
        form,
        selected,
        hover_fill,
        compact,
        icon_only,
        true,
        m,
        cx,
    )
    .when(form != Form::Rail, |row| {
        row.on_hover(move |hovered, window, cx| on_hover(hovered, window, cx))
    })
    .child({
        let mut tab = tab
            .h_full()
            .rounded(surface_radius(form, cx))
            .when(form == Form::Rail, |tab| {
                tab.flex_1().min_w_0().justify_start()
            });
        let corners = tab.style().corner_radii.clone();
        crate::motion::focus_ring::Ringed::new(tab, &focus, corners, cx)
    })
    .child(div().flex_none().flex().items_center().child(trailing));
    let row = glide.measure(key.clone(), row);
    match context_menu {
        Some(builder) => row
            .context_menu(move |menu, window, cx| builder(menu, window, cx))
            .into_any_element(),
        None => row.into_any_element(),
    }
}

/// The visible surface of one tab: the tab itself, or the row holding the tab
/// and its trailing control.
#[allow(clippy::too_many_arguments)]
fn surface<E: Styled + InteractiveElement + ParentElement + FluentBuilder>(
    el: E,
    form: Form,
    selected: bool,
    hover_fill: Hsla,
    compact: bool,
    icon_only: bool,
    trailing: bool,
    m: &Metrics,
    cx: &App,
) -> E {
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
        Form::Rail => material::selectable_row_under_mark(el, selected, compact, cx)
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
                    return segment(this, hover_fill, compact, cx);
                }
                this.h(px(m.item))
                    .rounded(surface_radius(form, cx))
                    .bg(hover_fill)
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

/// The space between a track's segments.
const SEGMENT_GAP: f32 = 2.;

/// A segment's corners sit concentric inside the track's.
fn segment_radius(cx: &App) -> Pixels {
    material::radius_input(cx) - px(TRACK_INSET)
}

/// The list of segments of a [`TabBar::segmented`] or a
/// [`ChoiceGroup`](super::choice::ChoiceGroup), for [`segmented_track`]: a
/// pill track whose segments divide the width it is given.
pub(crate) fn segment_list<E: Styled>(list: E, cx: &App) -> E {
    list.flex()
        .items_center()
        .h_full()
        .min_w_full()
        .flex_shrink_0()
        .gap(px(SEGMENT_GAP))
        .p(px(TRACK_INSET))
        .rounded(material::radius_input(cx))
        .bg(cx.theme().secondary)
}

/// The viewport of a [`segment_list`], which scrolls sideways once its
/// segments no longer fit.
pub(crate) fn segmented_track(
    id: impl Into<ElementId>,
    list: impl IntoElement,
    compact: bool,
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
        .child(list)
}

/// The selected segment's pill.
fn raised_segment(cx: &App) -> Hsla {
    let theme = cx.theme();
    // The popover surface is the lightest plane in the light theme but
    // matches the track in the dark one, where the pill is lifted with the
    // foreground instead.
    theme.by_mode(theme.popover, theme.foreground.opacity(0.12))
}

/// One segment's surface: `hover_fill` over the track, under the track's
/// gliding [`Glide::pill`].
pub(crate) fn segment<E: Styled>(el: E, hover_fill: Hsla, compact: bool, cx: &App) -> E {
    el.relative()
        .h(px(segment_height(compact)))
        .flex_grow(1.)
        .flex_basis(px(0.))
        .flex_shrink_0()
        .justify_center()
        .rounded(segment_radius(cx))
        .bg(hover_fill)
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
                    .trailing(move |_, _| {
                        let close = close.clone();
                        crate::widgets::Button::new("close")
                            .debug_selector(|| "close".into())
                            .label("x")
                            .on_click(move |_, _, _| close.set((close.get().0, close.get().1 + 1)))
                            .into_any_element()
                    }),
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
