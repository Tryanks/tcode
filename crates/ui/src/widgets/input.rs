use crate::{
    sizing::{Sizable, Size},
    theme::ActiveTheme as _,
};
use gpui::{
    App, DefiniteLength, Edges, Entity, Focusable as _, IntoElement, ParentElement as _,
    RenderOnce, SharedString, StyleRefinement, Styled, TextAlign, Window,
    prelude::FluentBuilder as _, px, rems,
};
use gpui_base::StyledExt as _;
use gpui_base::{InputBase, RoleOverride};

pub use gpui_base::input::{Copy, InputEvent, InputState, SelectAll, TextareaState};

enum State {
    Input(Entity<InputState>),
    Textarea(Entity<TextareaState>),
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
impl Styled for Input {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Input {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (base, multi_line) = match &self.state {
            State::Input(state) => {
                state.update(cx, |state, cx| state.prepare(window, cx));
                (state.read(cx).base_state().clone(), false)
            }
            State::Textarea(state) => {
                state.update(cx, |state, cx| state.prepare(window, cx));
                (state.read(cx).base_state().clone(), true)
            }
        };
        let theme = cx.theme().clone();
        base.update(cx, |state, cx| {
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
        });
        let focused = base.read(cx).focus_handle(cx).is_focused(window) && !self.disabled;
        let placeholder = base.read(cx).presentation().placeholder.clone();
        InputBase::new(("input", base.entity_id()))
            .focused(focused)
            .disabled(self.disabled)
            .role(self.role)
            .styles(|styles| styles.focused(|style| style.border_color(theme.ring)))
            .when_some(
                self.aria_label
                    .or_else(|| (!placeholder.is_empty()).then_some(placeholder)),
                |this, label| this.accessibility_label(label),
            )
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
            .child(base)
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
        self.input
    }
}
