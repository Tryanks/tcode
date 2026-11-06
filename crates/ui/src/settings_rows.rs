//! The rows a settings form is built from: Settings and the remote panels
//! inside it compose their groups from these, so every form row reads and
//! behaves the same.

use std::{cell::Cell, rc::Rc};

use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    RenderOnce, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Toggled, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use crate::icon::{Icon, IconName};
use crate::motion::{GlideState, SizeGlide, owned::Owned, reflow::Reflow};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::switch::Switch;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// The width of a text field beside its row's label; gpui-base has no form-field width token.
pub const FIELD_WIDTH: f32 = 240.;

/// Max width of the settings content column, a reading measure for its forms.
pub const CONTENT_MAX_WIDTH: f32 = 768.;

/// The centred column a settings-form page scrolls: `column` at
/// [`CONTENT_MAX_WIDTH`] inside the page inset, which glides when the window
/// turns compact or wide on a clock `motion` keeps.
pub(crate) fn content_column(
    column: gpui::Div,
    compact: bool,
    motion: &Owned,
    window: &mut Window,
    cx: &App,
) -> gpui::Div {
    let spacing = &cx.theme().tokens.spacing;
    let inset = motion.transition(
        "content-inset",
        if compact {
            px(crate::material::COMPACT_PAGE_INSET)
        } else {
            spacing.xl
        },
        GLIDE,
        window,
        cx,
    );
    h_flex()
        .w_full()
        .justify_center()
        .px(inset)
        .py(spacing.xl)
        // Keep this width definite before capping it. Reversing these
        // constraints makes nested multiline inputs resolve their percentage
        // width to zero when the cap applies.
        .child(column.w(px(CONTENT_MAX_WIDTH)).max_w_full())
}

/// The top strip of a page whose body is a [`content_column`]: the body's
/// inset, so its [`content_header_column`] stays over the content at every
/// width.
pub fn content_header(strip: gpui_base::Toolbar, cx: &App) -> gpui_base::Toolbar {
    strip.px(cx.theme().tokens.spacing.xl).justify_center()
}

/// The column inside a [`content_header`], as wide as the content below it.
pub fn content_header_column() -> gpui::Div {
    h_flex().w(px(CONTENT_MAX_WIDTH)).max_w_full()
}

/// A captioned group of settings rows: the unit every settings page is
/// composed of.
pub fn settings_group(
    caption: impl Into<SharedString>,
    rows: Vec<AnyElement>,
    cx: &App,
) -> gpui::Div {
    v_flex()
        .child(crate::material::group_caption(caption, cx))
        .child(crate::material::group(cx).children(rows))
}

/// Keyed rows of a [`crate::material::grouped`] card, each built by `build`
/// and carrying the hairline above it, so a line comes, goes and moves with
/// its row instead of appearing or vanishing between two that are moving
/// ([`crate::motion::keyed`]). The first row's line sits under the card's
/// top border, which clips it, so the row that becomes first slides its
/// line out of sight as the one above it folds away.
pub fn grouped_keyed<T: Clone + 'static, C: std::borrow::BorrowMut<App>>(
    id: impl Into<gpui::ElementId>,
    items: impl IntoIterator<Item = (SharedString, T)>,
    window: &mut Window,
    cx: &mut C,
    mut build: impl FnMut(&SharedString, &T, &mut Window, &mut C) -> AnyElement,
) -> AnyElement {
    crate::motion::keyed(id, items, ROWS, window, cx, |key, value, window, cx| {
        let row = build(key, value, window, cx);
        hairlined(row, std::borrow::Borrow::<App>::borrow(&*cx))
    })
    .render(v_flex().w_full().mt(px(-1.)))
}

/// `row` with the hairline a [`crate::material::grouped`] card draws above
/// it, for a row that comes and goes under another.
pub fn hairlined(row: impl IntoElement, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .child(crate::material::row_hairline(
            px(crate::material::CARD_INSET),
            cx,
        ))
        .child(row)
        .into_any_element()
}

/// The note under a [`settings_group`]'s card, aligned with its rows' text:
/// what holds for the group as a whole.
pub fn footnote(text: impl Into<SharedString>, compact: bool, cx: &App) -> gpui::Div {
    div()
        .pl(px(crate::material::CARD_INSET))
        .pt(cx.theme().tokens.spacing.sm)
        .text_secondary(compact, cx)
        .child(text.into())
}

/// A link off Tcode, beside the text it adds to.
pub fn external_link(id: impl Into<SharedString>, label: SharedString, url: String) -> Button {
    Button::new(id.into())
        .ghost()
        .xsmall()
        .icon(IconName::ExternalLink)
        .label(label)
        .on_click(move |_, _, cx| cx.open_url(&url))
}

/// Left description block (title + muted description). `reset` is the
/// per-item "restore default" affordance and rides inline right after the
/// title, so it reads as belonging to that setting rather than to the row's
/// control on the far right. A row without a description is one whose title
/// and control already say everything.
pub fn row_labels(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    reset: impl Into<Option<AnyElement>>,
    compact: bool,
    cx: &App,
) -> gpui::Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(
            h_flex()
                .min_w_0()
                .items_center()
                // A long title wraps rather than running under the control.
                .child(div().min_w_0().text_body(compact, cx).child(title.into()))
                .children(reset.into()),
        )
        .children(description.map(|desc| div().text_secondary(compact, cx).child(desc)))
}

/// The per-item "restore default" button: icon-only, ghost, and shown only
/// while `overridden` — a row showing its factory value has nothing to
/// restore, so the affordance would just be noise. It fades in when a value
/// is changed and out when it is restored.
pub fn reset_action(
    id: impl Into<gpui::ElementId>,
    overridden: bool,
    on_reset: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    ResetAction {
        id: id.into(),
        overridden,
        on_reset: Rc::new(on_reset),
    }
    .into_any_element()
}

#[derive(IntoElement)]
struct ResetAction {
    id: gpui::ElementId,
    overridden: bool,
    on_reset: crate::widgets::button::ClickHandler,
}

impl RenderOnce for ResetAction {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Some(shown) = crate::motion::shown(
            (self.id.clone(), "reset"),
            self.overridden.then_some(()),
            SHOW,
            window,
            cx,
        ) else {
            return div().into_any_element();
        };
        let label = crate::tr!("settings.reset_item").into_owned();
        let on_reset = self.on_reset;
        // Its own leading room, so a row with nothing to restore keeps none.
        shown.fade(
            div().pl_1().child(
                Button::new(self.id)
                    .ghost()
                    .xsmall()
                    .icon(IconName::Undo)
                    .tooltip(label.clone())
                    .aria_label(label)
                    .on_click(move |event, window, cx| {
                        // Toggle rows are clickable as a whole; a click that
                        // lands on this button must not also flip the switch.
                        cx.stop_propagation();
                        on_reset(event, window, cx);
                    }),
            ),
        )
    }
}

/// A row whose control can squeeze its label: stacked on a compact window.
/// A row whose control is a fixed affordance (a switch) is a [`ToggleRow`].
/// A row on a page the window can turn compact while it shows is a
/// [`FlipRow`].
pub fn row_frame(compact: bool, cx: &App) -> gpui::Div {
    if compact {
        crate::material::group_row_stacked(cx)
    } else {
        crate::material::group_row(cx)
    }
}

/// A [`row_frame`] that moves rather than jumps when the window turns compact
/// or wide: its parts glide from where the other layout drew them and its
/// height glides to the new one's. The clocks are kept in the owner's
/// [`Owned`] store, because the window draws a page under another parent
/// when it turns, where element state would start over.
pub(crate) struct FlipRow {
    reflow: Reflow,
    height: GlideState,
    field: GlideState,
    fill: Rc<Cell<Pixels>>,
    /// A [`Self::stretch`]ed control's own width.
    own: Rc<Cell<Pixels>>,
    id: SharedString,
    /// The frame's inset on both sides, which a field under the labels
    /// does not fill.
    inset: Pixels,
    compact: bool,
}

/// The [`FlipRow`] `id` of the view whose clocks `motion` keeps.
pub(crate) fn flip_row(motion: &Owned, id: impl Into<SharedString>, compact: bool) -> FlipRow {
    let id = id.into();
    let reflow = motion.reflow(id.clone(), u64::from(compact));
    flip_row_with(motion, id, reflow, compact)
}

/// A [`flip_row`] whose parts also glide when the row narrowing wraps them
/// (a field going under its label), while it stays where it is drawn.
#[cfg(feature = "remote-hosting")]
pub(crate) fn flip_row_in(
    motion: &Owned,
    id: impl Into<SharedString>,
    compact: bool,
    window: &mut Window,
    cx: &mut App,
) -> FlipRow {
    let id = id.into();
    let reflow = motion.reflow_in(id.clone(), u64::from(compact), window, cx);
    flip_row_with(motion, id, reflow, compact)
}

fn flip_row_with(motion: &Owned, id: SharedString, reflow: Reflow, compact: bool) -> FlipRow {
    FlipRow {
        id: id.clone(),
        reflow,
        height: motion.glide_state(format!("{id}-height")),
        field: motion.glide_state(format!("{id}-field")),
        fill: motion.keep(format!("{id}-fill")),
        own: motion.keep(format!("{id}-own")),
        inset: px(crate::material::CARD_INSET) * 2.,
        compact,
    }
}

impl FlipRow {
    pub(crate) fn compact(&self) -> bool {
        self.compact
    }

    /// The row's frame, for [`Self::render`].
    pub(crate) fn frame(&self, cx: &App) -> gpui::Div {
        row_frame(self.compact, cx)
    }

    /// `child` as the part `key` of the row, which glides to where the other
    /// layout puts it.
    pub(crate) fn part(&self, key: &'static str, child: impl IntoElement) -> AnyElement {
        self.reflow.part(key, child)
    }

    /// The common row: `labels` beside `control`, or over it.
    pub(crate) fn row(
        &self,
        labels: impl IntoElement,
        control: impl IntoElement,
        cx: &App,
    ) -> gpui::Div {
        self.frame(cx)
            .child(self.part("labels", labels))
            .child(self.part("control", control))
    }

    /// A text field: [`FIELD_WIDTH`] beside the labels, the row's width
    /// under them, its width gliding from one to the other. gpui has no
    /// layout transition, so the width the row gives is measured here (as
    /// the preview page's size is) and eased toward.
    pub(crate) fn field(&self, field: impl IntoElement, cx: &App) -> gpui::Div {
        self.glide_field(
            field,
            (self.fill.get(), |frame| frame.w_full()),
            (px(FIELD_WIDTH), |frame| frame.w(px(FIELD_WIDTH))),
            cx,
        )
    }

    /// A text field in a row of controls that fills the row under the
    /// labels: [`FIELD_WIDTH`] beside them, and what `beside` (the other
    /// controls, gaps included) leaves of the row under them.
    #[cfg(feature = "remote-hosting")]
    pub(crate) fn field_among(
        &self,
        field: impl IntoElement,
        beside: Pixels,
        cx: &App,
    ) -> gpui::Div {
        self.glide_field(
            field,
            (self.fill.get() - beside, |frame| frame.flex_1()),
            (px(FIELD_WIDTH), |frame| frame.w(px(FIELD_WIDTH))),
            cx,
        )
    }

    /// A control as wide as the row under the labels, and as wide as itself
    /// beside them, its width gliding between the two. Its own width is the
    /// one it last had at rest beside the labels; until it has been drawn
    /// there it takes either width at once.
    pub(crate) fn stretch(&self, control: impl IntoElement, cx: &App) -> gpui::Div {
        let frame = self.glide_field(
            control,
            (self.fill.get(), |frame| frame.w_full()),
            (self.own.get(), |frame| frame),
            cx,
        );
        let compact = self.compact;
        let (own, state) = (self.own.clone(), self.field.clone());
        gpui_base::ElementExt::on_prepaint(frame, move |bounds, _, cx| {
            if !compact && !state.moving(cx.background_executor().now()) {
                own.set(bounds.size.width);
            }
        })
    }

    /// `field` at the width its layout gives it, `(target, style)` for each
    /// layout, drawn gliding from the width it was drawn at when the layout
    /// changes. A target not yet measured takes the new width at once.
    fn glide_field(
        &self,
        field: impl IntoElement,
        compact: (Pixels, fn(gpui::Div) -> gpui::Div),
        inline: (Pixels, fn(gpui::Div) -> gpui::Div),
        cx: &App,
    ) -> gpui::Div {
        let (target, at_rest) = if self.compact { compact } else { inline };
        let width = self
            .field
            .toward(
                u64::from(self.compact),
                gpui::size(target, Pixels::ZERO),
                cx,
            )
            .width;
        let state = self.field.clone();
        // The field is laid out at its layout's own width, so what is beside
        // it never makes room for a width it is only passing through, and
        // is drawn at the gliding one; at rest the two are the same.
        let id = self.id.clone();
        let drawn = div()
            .debug_selector(move || format!("{id}-field"))
            .when(width != target && target > Pixels::ZERO, |drawn| {
                drawn.flex_none().w(width)
            })
            .child(field);
        let drawn = gpui_base::ElementExt::on_prepaint(drawn, move |bounds, window, _| {
            state.painted_at(bounds.size, window)
        });
        at_rest(div()).max_w_full().child(drawn)
    }

    /// The row, laid out as `row`.
    pub(crate) fn render(self, row: impl IntoElement) -> AnyElement {
        let fill = self.fill.clone();
        let inset = self.inset;
        let row =
            gpui_base::ElementExt::on_prepaint(div().w_full().child(row), move |bounds, _, _| {
                fill.set(bounds.size.width - inset);
            });
        SizeGlide::new(&self.height, self.reflow.render(row))
            .on_change(u64::from(self.compact))
            .into_any_element()
    }
}

/// A settings row whose control is a switch. The whole row is the control:
/// it owns click, focus, Space activation and the toggled state, and the
/// switch inside it only draws that state.
type ToggleHandler = Rc<dyn Fn(&bool, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct ToggleRow {
    id: SharedString,
    title: SharedString,
    description: Option<SharedString>,
    error: Option<SharedString>,
    checked: bool,
    compact: bool,
    disabled: bool,
    reset: Option<AnyElement>,
    accessory: Option<AnyElement>,
    on_toggle: ToggleHandler,
}

impl ToggleRow {
    /// `on_toggle` receives the state the row is being switched to.
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        checked: bool,
        compact: bool,
        on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: None,
            error: None,
            checked,
            compact,
            disabled: false,
            reset: None,
            accessory: None,
            on_toggle: Rc::new(on_toggle),
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Why the switch did not take, under the title in the danger colour.
    pub fn error(mut self, error: Option<SharedString>) -> Self {
        self.error = error;
        self
    }

    pub fn reset(mut self, reset: impl Into<Option<AnyElement>>) -> Self {
        self.reset = reset.into();
        self
    }

    /// Status shown between the labels and the switch. A clickable
    /// accessory stops its click from reaching the row.
    pub fn accessory(mut self, accessory: Option<AnyElement>) -> Self {
        self.accessory = accessory;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl RenderOnce for ToggleRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            title,
            description,
            error,
            checked,
            compact,
            disabled,
            reset,
            accessory,
            on_toggle,
        } = self;
        // The row is the tab stop, but the inline reset button is one too, so
        // the row owns an explicit handle (`accessible_clickable`'s implicit one
        // cannot be queried) and applies the tab order to it by hand.
        let focus = window
            .use_keyed_state(SharedString::from(format!("{id}-focus")), cx, |_, cx| {
                cx.focus_handle().tab_stop(true).tab_index(0)
            })
            .read(cx)
            .clone();
        let row_focus = focus.clone();
        let key_toggle = on_toggle.clone();
        let has_accessory = accessory.is_some();
        let error = crate::motion::shown(
            SharedString::from(format!("{id}-error")),
            error,
            SHOW,
            window,
            cx,
        );
        let selector = id.clone();
        crate::material::accessible_clickable(
            crate::material::group_row(cx),
            SharedString::from(format!("{id}-row")),
            Role::Switch,
            title.clone(),
            cx,
        )
        .debug_selector(move || selector.to_string())
        .track_focus(&focus)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .when(!disabled, |row| {
            row.cursor_pointer()
                // Handle Space during capture so the native event cannot fall
                // through to scrolling/text input before the row's synthesized
                // click runs. Enter continues to use GPUI's standard
                // focused-click behavior.
                .capture_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                    // Capture runs from the root down, so without this guard
                    // the row would swallow Space aimed at the reset button
                    // inside it.
                    if !row_focus.is_focused(window) {
                        return;
                    }
                    if event.keystroke.key == "space"
                        && !event.is_held
                        && !event.keystroke.modifiers.modified()
                    {
                        window.prevent_default();
                        cx.stop_propagation();
                        key_toggle(&!checked, window, cx);
                    }
                })
                .on_click(move |_, window, cx| on_toggle(&!checked, window, cx))
        })
        .child(
            row_labels(title, description, reset, compact, cx)
                .children(error.map(|error| {
                    error.fold(
                        div()
                            .text_secondary(compact, cx)
                            .text_color(cx.theme().danger_foreground)
                            .child(error.value.clone()),
                    )
                }))
                // An accessory takes the room the labels leave.
                .when(has_accessory, |labels| labels.flex_none()),
        )
        .children(accessory)
        .child(Switch::new(id).checked(checked).disabled(disabled))
    }
}

/// The trigger of a settings select.
pub fn dropdown_trigger(id: &'static str, label: impl Into<SharedString>, cx: &App) -> Button {
    Button::new(id).ghost().outline().compact().child(
        h_flex()
            // One width for every select trigger, so a column of them lines up; gpui-base's
            // Select is not adopted yet.
            .w(px(160.))
            .items_center()
            .justify_between()
            .gap_2()
            .child(label.into())
            .child(Icon::new(IconName::ChevronDown).text_color(cx.theme().muted_foreground)),
    )
}

/// One choice of a [`select_row`].
#[derive(Clone)]
pub struct SelectRowOption<T> {
    pub value: T,
    pub id: SharedString,
    pub label: SharedString,
    pub description: Option<SharedString>,
    pub selected: bool,
}

/// A settings row whose control picks one of `options` from a menu.
/// `menu_width` is the menu's floor, so its longest option fits.
#[allow(clippy::too_many_arguments)]
pub(crate) fn select_row<T: Clone + 'static>(
    trigger_id: &'static str,
    popover_id: &'static str,
    menu_id: &'static str,
    menu_width: f32,
    title: SharedString,
    description: Option<SharedString>,
    selected_label: SharedString,
    options: Vec<SelectRowOption<T>>,
    reset: impl Into<Option<AnyElement>>,
    row: FlipRow,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let compact = row.compact();
    let trigger = dropdown_trigger(trigger_id, selected_label, cx);
    let on_select = Rc::new(on_select);
    let menu_label = title.clone();
    let dropdown = crate::material::overlay_popover(popover_id, cx)
        .trigger(trigger)
        .content(move |_, _, cx| {
            v_flex()
                .id(menu_id)
                .role(Role::Menu)
                .aria_label(menu_label.clone())
                .p_1()
                // Each menu names its own floor so its longest option fits; gpui-base's
                // Select is not adopted yet.
                .min_w(px(menu_width))
                .gap_0p5()
                .children(options.clone().into_iter().map(|option| {
                    let popover = cx.entity();
                    let on_select = on_select.clone();
                    let label = option.label.clone();
                    let item = crate::material::accessible_clickable(
                        crate::material::menu_item_frame(
                            compact,
                            crate::material::ItemFill::Hover,
                            cx,
                        ),
                        option.id,
                        Role::MenuItem,
                        label.clone(),
                        cx,
                    )
                    .aria_selected(option.selected)
                    .w_full()
                    .text_body(compact, cx)
                    .cursor_pointer();
                    let item = if let Some(description) = option.description {
                        item.items_start().child(
                            v_flex()
                                .flex_1()
                                .gap_0p5()
                                .child(div().child(label))
                                .child(div().text_secondary(compact, cx).child(description)),
                        )
                    } else {
                        item.child(div().flex_1().child(label))
                    };
                    item.when(option.selected, |item| {
                        item.child(Icon::new(IconName::Check))
                    })
                    .on_click(move |_, window, cx| {
                        on_select(option.value.clone(), window, cx);
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                    })
                }))
        });

    let layout = row.row(
        row_labels(title, description, reset, compact, cx),
        dropdown,
        cx,
    );
    row.render(layout)
}
