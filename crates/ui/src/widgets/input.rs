#[path = "input_configuration.rs"]
mod input_configuration;

use crate::{
    sizing::{Sizable, Size},
    theme::ActiveTheme as _,
    touch_selection::{EditMenuItem, TouchSelectionOverlay},
};
use gpui::{
    Action, AnyElement, App, DefiniteLength, Edges, Entity, Focusable as _, IntoElement,
    LongPressEvent, ParentElement as _, RenderOnce, SharedString, StyleRefinement, Styled,
    TextAlign, TouchPhase, Window, div, prelude::FluentBuilder as _, px, rems,
};
use gpui_base::StyledExt as _;
use gpui_base::{InputBase, RoleOverride};

pub use gpui_base::input::{Copy, InputEvent, InputState, Paste, SelectAll, TextareaState};

#[derive(Clone)]
enum State {
    Input(Entity<InputState>),
    Textarea(Entity<TextareaState>),
}

macro_rules! dispatch_state {
    ($state:expr, |$concrete:ident| $body:expr) => {
        match $state {
            State::Input($concrete) => $body,
            State::Textarea($concrete) => $body,
        }
    };
}

#[derive(IntoElement)]
pub struct Input {
    state: State,
    style: StyleRefinement,
    size: Size,
    height: Option<DefiniteLength>,
    appearance: bool,
    bordered: bool,
    disabled: bool,
    tab_index: isize,
    role: RoleOverride,
    aria_label: Option<SharedString>,
}

impl Input {
    pub fn new(state: &Entity<InputState>) -> Self {
        Self::with_state(State::Input(state.clone()))
    }
    fn from_textarea(state: &Entity<TextareaState>) -> Self {
        Self::with_state(State::Textarea(state.clone()))
    }
    fn with_state(state: State) -> Self {
        Self {
            state,
            style: StyleRefinement::default(),
            size: Size::Medium,
            height: None,
            appearance: true,
            bordered: true,
            disabled: false,
            tab_index: 0,
            role: RoleOverride::Implicit,
            aria_label: None,
        }
    }
    pub fn appearance(mut self, appearance: bool) -> Self {
        self.appearance = appearance;
        self
    }
    pub fn bordered(mut self, bordered: bool) -> Self {
        self.bordered = bordered;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn h(mut self, height: impl Into<DefiniteLength>) -> Self {
        self.height = Some(height.into());
        self
    }
    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = tab_index;
        self
    }
    pub fn role(mut self, role: impl Into<RoleOverride>) -> Self {
        self.role = role.into();
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
}

impl Sizable for Input {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

/// The handles and the edit menu of the selection a long press made.
///
/// The menu offers what the native context menu would: Cut, Copy, Paste and
/// Select All, leaving out what cannot apply right now rather than disabling
/// it. Cut, Copy and Paste go through the input's actions, so a custom key
/// binding or a capture handler above the input (the composer's image paste)
/// sees them the same way.
fn render_touch_selection(state: &State, window: &Window, cx: &App) -> Vec<AnyElement> {
    if dispatch_state!(state, |base| base.read(cx).touch_selection()).is_none() {
        return Vec::new();
    }
    let entity_id = dispatch_state!(state, |base| base.entity_id());
    let (capabilities, selectable, focus_handle) = dispatch_state!(state, |base| {
        let base = base.read(cx);
        (
            base.context_menu_capabilities(),
            // Select All has nothing left to offer once every character is
            // selected.
            base.text().len() > 0 && base.selected_range() != (0..base.text().len()),
            base.presentation().focus_handle().clone(),
        )
    });
    let editable = capabilities.is_editable();
    let copyable = capabilities.is_copyable();
    // Offered whenever the text can change, without peeking at the
    // clipboard: on iOS every read of it shows the system's paste banner,
    // and an empty clipboard pastes nothing.
    let pasteable = editable;

    let dispatch = move |action: &dyn Action, window: &mut Window, cx: &mut App| {
        focus_handle.dispatch_action(action, window, cx);
    };
    let mut items = Vec::with_capacity(4);
    if editable && copyable {
        let dispatch = dispatch.clone();
        items.push(EditMenuItem::new(
            "cut",
            crate::tr!("edit_menu.cut").into_owned(),
            move |window, cx| dispatch(&gpui_base::input::Cut, window, cx),
        ));
    }
    if copyable {
        let dispatch = dispatch.clone();
        let state = state.clone();
        items.push(EditMenuItem::new(
            "copy",
            crate::tr!("edit_menu.copy").into_owned(),
            move |window, cx| {
                dispatch(&Copy, window, cx);
                dispatch_state!(&state, |base| base
                    .update(cx, |base, cx| base.close_edit_menu(cx)));
            },
        ));
    }
    if pasteable {
        let dispatch = dispatch.clone();
        items.push(EditMenuItem::new(
            "paste",
            crate::tr!("edit_menu.paste").into_owned(),
            move |window, cx| dispatch(&Paste, window, cx),
        ));
    }
    if selectable {
        let state = state.clone();
        items.push(EditMenuItem::new(
            "select-all",
            crate::tr!("edit_menu.select_all").into_owned(),
            move |window, cx| {
                dispatch_state!(&state, |base| base
                    .update(cx, |base, cx| base.select_all_from_edit_menu(window, cx)))
            },
        ));
    }

    let drag_state = state.clone();
    let source_state = state.clone();
    TouchSelectionOverlay::new(("input-touch-selection", entity_id), move |_, cx| {
        dispatch_state!(&source_state, |base| base.read(cx).touch_selection())
    })
    .handles(move |edge, phase, position, _, cx| {
        dispatch_state!(&drag_state, |base| base.update(
            cx,
            |base, cx| match phase {
                TouchPhase::Started => base.begin_edge_drag(edge, position, cx),
                TouchPhase::Moved => base.update_edge_drag(position, cx),
                TouchPhase::Ended | TouchPhase::Cancelled => base.end_edge_drag(cx),
            }
        ))
    })
    .items(items)
    .into_elements(window, cx)
}
impl Styled for Input {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Input {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let multi_line = matches!(self.state, State::Textarea(_));
        dispatch_state!(&self.state, |state| state
            .update(cx, |state, cx| state.prepare(window, cx)));
        let theme = cx.theme().clone();
        dispatch_state!(&self.state, |base| base.update(cx, |state, cx| {
            state.set_editor_style(gpui_base::input::InputEditorStyle {
                foreground: theme.foreground,
                muted_foreground: theme.muted_foreground,
                background: theme.background,
                border: theme.border,
                selection: theme.selection,
                caret: theme.foreground,
                highlight_styles: theme.highlight_theme.clone(),
                ..Default::default()
            });
            state.set_editor_paddings(if multi_line {
                Edges::all(px(8.))
            } else {
                Edges::default()
            });
            state.set_disabled(self.disabled, cx);
            state.set_text_align(self.style.text.text_align.unwrap_or(TextAlign::Left), cx);
        }));
        let focused = dispatch_state!(&self.state, |base| base
            .read(cx)
            .focus_handle(cx)
            .is_focused(window))
            && !self.disabled;
        let placeholder = dispatch_state!(&self.state, |base| base
            .read(cx)
            .presentation()
            .placeholder()
            .clone());
        let aria_label = self
            .aria_label
            .or_else(|| (!placeholder.is_empty()).then_some(placeholder));
        // The selection's controls belong to the field the finger is in:
        // a field that lost focus to another keeps its selection, as any
        // input does, but not the handles and the menu over it.
        let touch_selection = if focused {
            render_touch_selection(&self.state, window, cx)
        } else {
            Vec::new()
        };
        dispatch_state!(&self.state, |base| {
            let editor = if multi_line {
                // Give the editor a definite viewport before InputBase measures its
                // soft-wrap width. Mounting it directly as a horizontal flex child
                // lets mixed-script intrinsic widths leak into that measurement.
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .min_w_0()
                    .child(div().relative().flex_1().min_w_0().child(base.clone()))
                    .into_any_element()
            } else {
                base.clone().into_any_element()
            };
            let input_entity = base.clone();
            let focus = base.read(cx).focus_handle(cx);
            InputBase::new(("input", base.entity_id()))
                .focused(focused)
                .disabled(self.disabled)
                .role(self.role)
                .styles(|styles| styles.focused(|style| style.border_color(theme.ring)))
                .when_some(aria_label.clone(), |this, label| {
                    this.accessibility_label(label)
                })
                .relative()
                .flex()
                .size_full()
                .items_center()
                // The editor inherits the ambient text style, so typography must
                // be pinned here (upstream gpui-component convention): explicit
                // per-size font size and a fixed line height, not one relative to
                // whatever font size happens to cascade in.
                .line_height(rems(1.25))
                .map(|this| match self.size {
                    Size::XSmall => this.text_xs(),
                    Size::Small | Size::Medium => this.text_sm(),
                    Size::Large => this.text_base(),
                    Size::Size(v) => this.text_size(v * 0.875),
                })
                .when(!multi_line, |this| match self.size {
                    Size::XSmall => this.h_5().px_1(),
                    Size::Small => this.h_6().px_2(),
                    Size::Large => this.h_10().px_3(),
                    Size::Size(v) => this.h(v).px(v * 0.2),
                    Size::Medium => this.h_8().px_2p5(),
                })
                // Multi-line insets come from the editor paddings above; padding
                // the container as well doubles them.
                .when(multi_line, |this| {
                    this.h_auto()
                        .when_some(self.height, |this, height| this.h(height))
                })
                .when(self.appearance, |this| {
                    this.bg(theme.background)
                        .rounded(theme.radius)
                        .when(self.bordered, |this| {
                            this.border_1().border_color(theme.input)
                        })
                })
                .refine_style(&self.style)
                .child(editor)
                // Register after the editor paints so GPUI uses its configured handler.
                .child(
                    gpui::canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            // A long press here selects in this field; the
                            // window selection a message holds goes, as it
                            // would on a tap. No tap precedes a long press.
                            window.on_mouse_event(
                                move |event: &LongPressEvent, phase, window, cx| {
                                    if phase.bubble()
                                        && event.phase == TouchPhase::Started
                                        && !window.default_prevented()
                                        && bounds.contains(&event.start_position)
                                    {
                                        gpui_base::TextSelection::clear(window, cx);
                                    }
                                },
                            );
                            let bounds = input_entity.read(cx).text_bounds().unwrap_or(bounds);
                            window.handle_input(
                                &focus,
                                input_configuration::ConfiguredInput::new(
                                    bounds,
                                    input_entity.clone(),
                                    multi_line,
                                ),
                                cx,
                            );
                        },
                    )
                    .absolute()
                    .size_full(),
                )
                // The handles float above the field and the menu above the
                // window; both are deferred, so they draw over the editor.
                .children(touch_selection)
                .into_any_element()
        })
    }
}

#[derive(IntoElement)]
pub struct Textarea {
    input: Input,
}
impl Textarea {
    pub fn new(state: &Entity<TextareaState>) -> Self {
        Self {
            input: Input::from_textarea(state),
        }
    }
    pub fn appearance(mut self, value: bool) -> Self {
        self.input = self.input.appearance(value);
        self
    }
    pub fn bordered(mut self, value: bool) -> Self {
        self.input = self.input.bordered(value);
        self
    }
    pub fn disabled(mut self, value: bool) -> Self {
        self.input = self.input.disabled(value);
        self
    }
    pub fn h(mut self, value: impl Into<DefiniteLength>) -> Self {
        self.input = self.input.h(value);
        self
    }
}
impl Styled for Textarea {
    fn style(&mut self) -> &mut StyleRefinement {
        self.input.style()
    }
}
impl RenderOnce for Textarea {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let State::Textarea(state) = &self.input.state else {
            unreachable!()
        };
        let handle = crate::touch_scroll::Handle::Textarea(state.clone());
        let id = state.entity_id();
        crate::touch_scroll::register(self.input, handle).with_id(("textarea-scroll", id))
    }
}
