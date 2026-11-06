//! Adding a platform: its kind and its address, checked by the kind's own rule, then straight on
//! to connecting it in the same dialog.

use super::{HostEditor, mark, slug};
use crate::{
    overlay::{DialogActions, OverlayExt as _},
    sizing::Sizable as _,
    store::WorkspaceStore,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::{Radio, RadioGroup, h_flex, v_flex};
use tcode_core::pull_request::{HostKind, HostRefusal};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// What Add's input holds for a kind before anything is typed: its public host, or nothing
/// where a platform is an organization the user names.
fn prefill(kind: HostKind) -> &'static str {
    if kind.organizations() {
        ""
    } else {
        kind.public_host()
    }
}

struct AddHost {
    store: Entity<WorkspaceStore>,
    listed: Vec<String>,
    kind: HostKind,
    input: Entity<InputState>,
    error: Option<String>,
    /// The platform added, being connected.
    connecting: Option<Entity<HostEditor>>,
    _subscription: Subscription,
}

impl AddHost {
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().to_string();
        let name = self.kind.terms().name;
        let organizations = self.kind.organizations();
        let host = match self.kind.authority(&typed) {
            Ok(host) => host,
            Err(refusal) => {
                self.error = Some(
                    match refusal {
                        HostRefusal::Blank if organizations => {
                            crate::tr!("source_control.error_blank_organization")
                        }
                        HostRefusal::Invalid if organizations => {
                            crate::tr!("source_control.error_invalid_organization")
                        }
                        HostRefusal::Blank => crate::tr!("source_control.error_blank"),
                        HostRefusal::Invalid => crate::tr!("source_control.error_invalid"),
                        HostRefusal::PortOrPath => {
                            crate::tr!("source_control.error_port_path", kind = name)
                        }
                        HostRefusal::Path => crate::tr!("source_control.error_path", kind = name),
                        HostRefusal::Fixed => crate::tr!(
                            "source_control.error_fixed",
                            kind = name,
                            host = self.kind.public_host()
                        ),
                    }
                    .into_owned(),
                );
                return cx.notify();
            }
        };
        if self.listed.contains(&host) {
            self.error = Some(crate::tr!("source_control.error_exists", host = &host).into_owned());
            return cx.notify();
        }
        let kind = self.kind;
        self.store.update(cx, |store, _| {
            store.patch_source_control_host(host.clone(), kind, Some(true), None)
        });
        let store = self.store.clone();
        let editor = cx.new(|cx| HostEditor::new(store, host, true, false, window, cx));
        editor.update(cx, |editor, cx| editor.focus_token(window, cx));
        self.connecting = Some(editor);
        cx.notify();
    }

    fn choose(&mut self, kind: HostKind, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().trim().to_ascii_lowercase();
        let previous = self.kind;
        self.kind = kind;
        self.error = None;
        self.input.update(cx, |input, cx| {
            input.set_placeholder(
                crate::tr!(&format!("source_control.host_placeholder_{}", slug(kind))).into_owned(),
                window,
                cx,
            );
            if typed.is_empty() || typed == previous.public_host() || kind.fixed_host().is_some() {
                input.set_value(prefill(kind), window, cx);
            }
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// One kind as a tile: its mark and name over what of it Tcode reaches.
    fn tile(&self, index: usize, kind: HostKind, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let checked = kind == self.kind;
        let id = SharedString::from(format!("source-control-kind-{}", slug(kind)));
        let selector = id.to_string();
        Radio::new(id)
            .debug_selector(move || selector)
            .checked(checked)
            .set_position(index + 1, HostKind::ALL.len())
            .accessibility_label(kind.terms().name)
            .flex()
            .flex_col()
            .gap_0p5()
            .p_2()
            .rounded(crate::material::radius_input(cx))
            .border_1()
            .border_color(if checked { theme.primary } else { theme.border })
            .when(checked, |tile| tile.bg(theme.primary.opacity(0.1)))
            .cursor_pointer()
            .on_change({
                let add = cx.entity().downgrade();
                move |_, _, window, cx| {
                    let _ = add.update(cx, |add, cx| add.choose(kind, window, cx));
                }
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_body(false, cx)
                    .child(mark(kind).flex_none())
                    .child(kind.terms().name),
            )
            .child(
                div().text_secondary(false, cx).child(
                    crate::tr!(&format!("source_control.cover_{}", slug(kind))).into_owned(),
                ),
            )
            .into_any_element()
    }
}

impl Render for AddHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(editor) = &self.connecting {
            return div().w_full().child(editor.clone());
        }
        let compact = crate::window_seam::window_is_compact(window, cx);
        let error = crate::motion::shown(
            "source-control-add-error",
            self.error.clone(),
            SHOW,
            window,
            cx,
        );
        let theme = cx.theme().clone();
        let kind = self.kind;
        let tiles: Vec<_> = HostKind::ALL
            .into_iter()
            .enumerate()
            .map(|(index, kind)| self.tile(index, kind, cx))
            .collect();
        let label = |text: SharedString| div().text_caption(cx).child(text);
        div().w_full().child(
            v_flex()
                .gap_2()
                .child(label(
                    crate::tr!("source_control.kind_label").into_owned().into(),
                ))
                .child(
                    RadioGroup::new("source-control-kinds")
                        .aria_label(crate::tr!("source_control.kind_label").into_owned())
                        .grid()
                        .grid_cols(if compact { 2 } else { 3 })
                        .gap_2()
                        .children(tiles),
                )
                .child(
                    div().pt_1().child(label(
                        if kind.organizations() {
                            crate::tr!("source_control.organization_label")
                        } else {
                            crate::tr!("source_control.host_label")
                        }
                        .into_owned()
                        .into(),
                    )),
                )
                // A kind served from one host alone takes no other.
                .child(
                    div()
                        .debug_selector(|| "source-control-address".into())
                        .child(Input::new(&self.input).disabled(kind.fixed_host().is_some())),
                )
                .child(
                    div()
                        .text_secondary(compact, cx)
                        .flex()
                        .flex_wrap()
                        .children(
                            crate::tr!(&format!("source_control.note_{}", slug(kind)))
                                .split_inclusive([' ', '。'])
                                .map(|word| {
                                    div()
                                        .min_w_0()
                                        .when(word.is_ascii() || word.contains('.'), |fragment| {
                                            fragment.whitespace_nowrap()
                                        })
                                        .child(word.to_owned())
                                }),
                        ),
                )
                .child(
                    div()
                        // Holds one line while empty, so an error does not move the dialog.
                        .min_h(theme.tokens.typography.xs.line_height)
                        .text_token(&theme.tokens.typography.xs)
                        .text_color(theme.danger)
                        .debug_selector(|| "source-control-add-error".into())
                        .children(error.map(|error| error.fade(div().child(error.value.clone())))),
                ),
        )
    }
}

/// Opens the Add dialog, which goes on to connect the platform once it is added.
pub(super) fn open(
    store: Entity<WorkspaceStore>,
    listed: Vec<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let add = cx.new(|cx| {
        let kind = HostKind::Github;
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!(&format!(
                "source_control.host_placeholder_{}",
                slug(kind)
            )))
        });
        input.update(cx, |input, cx| input.set_value(prefill(kind), window, cx));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |add: &mut AddHost, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    add.error = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => add.add(window, cx),
                _ => {}
            },
        );
        AddHost {
            store,
            listed,
            kind,
            input,
            error: None,
            connecting: None,
            _subscription: subscription,
        }
    });
    let focus = add.read(cx).input.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let content = add.clone();
        let dialog = dialog.content(move |el, _, _| el.child(content.clone()));
        if let Some(editor) = add.read(cx).connecting.clone() {
            return dialog
                .title(editor.read(cx).title(cx))
                .footer(HostEditor::footer(&editor, cx));
        }
        let confirm = add.clone();
        dialog
            .title(crate::tr!("source_control.add_host").into_owned())
            .footer(
                DialogActions::new()
                    .child(
                        Button::new("source-control-add-cancel")
                            .debug_selector(|| "source-control-add-cancel".into())
                            .outline()
                            .small()
                            .label(crate::tr!("settings.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("source-control-add-confirm")
                            .debug_selector(|| "source-control-add-confirm".into())
                            .primary()
                            .small()
                            .label(crate::tr!("source_control.continue"))
                            .on_click(move |_, window, cx| {
                                confirm.update(cx, |add, cx| add.add(window, cx))
                            }),
                    )
                    .into_any_element(),
            )
    });
    // The dialog takes focus as it opens; the address is what is typed first.
    focus.update(cx, |input, cx| input.focus(window, cx));
}
