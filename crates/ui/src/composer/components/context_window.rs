use super::super::*;
use crate::overlay::DialogActions;
use crate::store::ComposerState;

use agent::context_window;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// The context-window choice for the active model: its presets, each applied
/// as it is chosen, and a custom size.
pub(in super::super) struct ContextWindowDialog {
    store: Entity<WorkspaceStore>,
    /// The thread whose model the dialog sizes.
    session_id: Option<String>,
    custom: Entity<InputState>,
    invalid: bool,
    _custom_events: Subscription,
}

impl ContextWindowDialog {
    fn new(
        store: Entity<WorkspaceStore>,
        session_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (min, max) = custom_range(&store, session_id.as_deref(), cx);
        let placeholder = range_text(false, min, max);
        let custom = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let custom_events =
            cx.subscribe_in(&custom, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.apply_custom(window, cx),
                InputEvent::Change => {
                    this.invalid = false;
                    cx.notify();
                }
                _ => {}
            });
        Self {
            store,
            session_id,
            custom,
            invalid: false,
            _custom_events: custom_events,
        }
    }

    fn apply_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.custom.read(cx).value().to_string();
        let (min, max) = custom_range(&self.store, self.session_id.as_deref(), cx);
        let id = self
            .store
            .read(cx)
            .composer_state_for(self.session_id.as_deref(), cx);
        let Some((id, ..)) = context_window_descriptor(&id) else {
            return;
        };
        match context_window::parse_tokens(&serde_json::Value::String(value)).filter(|tokens| {
            min.is_none_or(|min| *tokens >= min) && max.is_none_or(|max| *tokens <= max)
        }) {
            Some(tokens) => {
                let session_id = self.session_id.clone().unwrap_or_default();
                self.store.update(cx, |store, _cx| {
                    store.set_active_option_for(session_id, id, Some(serde_json::json!(tokens)));
                });
                window.close_dialog(cx);
            }
            None => {
                self.invalid = true;
                cx.notify();
            }
        }
    }
}

impl Render for ContextWindowDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let composer = self
            .store
            .read(cx)
            .composer_state_for(self.session_id.as_deref(), cx);
        let Some((id, options, default_value)) = context_window_descriptor(&composer) else {
            return div().into_any_element();
        };
        let states = OptionStates::of(&composer);
        let selected = states.running(&id).and_then(context_window::parse_tokens);
        let pending = states.pending(&id).and_then(context_window::parse_tokens);
        let note = states.unconfirmed_note(&id);
        let tokens_of = |value: &str| context_window::parse_tokens(&serde_json::json!(value));
        let is_custom = |tokens: Option<u64>| {
            tokens.is_some_and(|tokens| {
                !options
                    .iter()
                    .any(|option| tokens_of(&option.value) == Some(tokens))
            })
        };
        let custom_selected = is_custom(selected);
        let pending_suffix = format!(" · {}", crate::tr!("permission.pending"));
        let compact = crate::window_seam::window_is_compact(window, cx);
        let invalid = crate::motion::shown(
            "context-window-invalid",
            self.invalid.then_some(()),
            SHOW,
            window,
            cx,
        );
        let primary = cx.theme().primary;
        let default_suffix = crate::tr!("composer.option_default").into_owned();

        let row = |id: gpui::SharedString, label: String, checked: bool, cx: &App| {
            crate::material::menu_item_frame(compact, crate::material::ItemFill::Hover, cx)
                .id(id)
                .w_full()
                .items_center()
                .cursor_pointer()
                .text_body(compact, cx)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(label)),
                )
                .when(checked, |row| {
                    row.child(Icon::new(IconName::Check).text_color(primary))
                })
        };

        let mut list = v_flex().w_full().gap_0p5();
        if let Some(note) = note {
            list = list.child(div().px_2().pb_1().text_secondary(compact, cx).child(note));
        }
        for (index, option) in options.iter().enumerate() {
            let mut label = option.label.clone();
            if default_value.as_deref() == Some(option.value.as_str()) {
                label.push_str(&default_suffix);
            }
            if pending.is_some() && tokens_of(&option.value) == pending {
                label.push_str(&pending_suffix);
            }
            let checked = selected.is_some() && tokens_of(&option.value) == selected;
            let store = self.store.clone();
            let session_id = self.session_id.clone().unwrap_or_default();
            let value = option.value.clone();
            let id = id.clone();
            list = list.child(
                row(format!("context-window-{index}").into(), label, checked, cx).on_click(
                    move |_, window, cx| {
                        let value = value.clone();
                        store.update(cx, |store, _cx| {
                            store.set_active_option_for(
                                session_id.clone(),
                                id.clone(),
                                Some(serde_json::Value::String(value)),
                            );
                        });
                        window.close_dialog(cx);
                    },
                ),
            );
        }
        let mut custom_label = crate::tr!("composer.context_window_custom").into_owned();
        if let Some(selected) = selected.filter(|_| custom_selected) {
            custom_label.push_str(&format!(" ({})", context_window::format(selected)));
        }
        if let Some(pending) = pending.filter(|_| is_custom(pending)) {
            custom_label.push_str(&format!(
                " · {} {}",
                context_window::format(pending),
                crate::tr!("permission.pending")
            ));
        }
        let custom = self.custom.clone();
        list.child(
            row(
                "context-window-custom".into(),
                custom_label,
                custom_selected,
                cx,
            )
            .on_click(move |_, window, cx| {
                custom.update(cx, |state, cx| state.focus(window, cx));
            }),
        )
        .child(
            v_flex()
                .px_2()
                .pt_1()
                .child(Input::new(&self.custom))
                // The error folds in under the field on Apply and away as
                // the field is edited.
                .children(invalid.map(|shown| {
                    shown.fold(
                        div()
                            .pt_1()
                            .text_secondary(compact, cx)
                            .text_color(cx.theme().danger)
                            .child({
                                let (min, max) =
                                    custom_range(&self.store, self.session_id.as_deref(), cx);
                                range_text(true, min, max)
                            }),
                    )
                })),
        )
        .into_any_element()
    }
}

/// The active model's context-window option: its id, presets and default,
/// when it has one.
pub(in super::super) fn context_window_descriptor(
    composer: &ComposerState,
) -> Option<(String, Vec<agent::SelectOption>, Option<String>)> {
    match context_window::descriptor(&composer.active_option_descriptors)? {
        OptionDescriptor::Select {
            id,
            options,
            default_value,
            ..
        } => Some((id.clone(), options.clone(), default_value.clone())),
        OptionDescriptor::Boolean { .. } => None,
    }
}

/// The sizes a custom window can take: at least the smallest the harness
/// says it takes, and at most the active model's largest preset, the window
/// it can launch with. An end the harness and the model leave open is open
/// here too, and the harness holds the size to what the model takes.
fn custom_range(
    store: &Entity<WorkspaceStore>,
    session_id: Option<&str>,
    cx: &App,
) -> (Option<u64>, Option<u64>) {
    let composer = store.read(cx).composer_state_for(session_id, cx);
    let descriptors = &composer.active_option_descriptors;
    let min = match context_window::descriptor(descriptors) {
        Some(OptionDescriptor::Select { minimum, .. }) => *minimum,
        _ => None,
    };
    (min, context_window::largest(descriptors))
}

/// The custom field's placeholder, or the error under it, for a range.
fn range_text(invalid: bool, min: Option<u64>, max: Option<u64>) -> String {
    let (min, max) = (
        min.map(context_window::format),
        max.map(context_window::format),
    );
    match (invalid, min, max) {
        (false, Some(min), Some(max)) => {
            crate::tr!(
                "composer.context_window_custom_placeholder",
                min = min,
                max = max
            )
        }
        (false, Some(min), None) => {
            crate::tr!("composer.context_window_custom_placeholder_open", min = min)
        }
        (false, None, Some(max)) => {
            crate::tr!(
                "composer.context_window_custom_placeholder_up_to",
                max = max
            )
        }
        (false, None, None) => crate::tr!("composer.context_window_custom_placeholder_any"),
        (true, Some(min), Some(max)) => {
            crate::tr!("composer.context_window_invalid", min = min, max = max)
        }
        (true, Some(min), None) => crate::tr!("composer.context_window_invalid_open", min = min),
        (true, None, Some(max)) => crate::tr!("composer.context_window_invalid_up_to", max = max),
        (true, None, None) => crate::tr!("composer.context_window_invalid_any"),
    }
    .into_owned()
}

/// Open the context-window dialog for the model of `session_id`'s thread.
pub(in super::super) fn open(
    store: Entity<WorkspaceStore>,
    session_id: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog = cx.new(|cx| ContextWindowDialog::new(store, session_id, window, cx));
    window.open_dialog(cx, move |builder, _, _| {
        let content = dialog.clone();
        let apply = dialog.clone();
        builder
            .sheet()
            .title(crate::tr!("composer.context_window_title").into_owned())
            .content(move |content_el, _, _| content_el.child(content.clone()))
            .footer(
                DialogActions::new()
                    .child(
                        Button::new("context-window-cancel")
                            .label(crate::tr!("composer.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("context-window-apply")
                            .primary()
                            .label(crate::tr!("composer.context_window_apply"))
                            .on_click(move |_, window, cx| {
                                apply.update(cx, |dialog, cx| dialog.apply_custom(window, cx));
                            }),
                    ),
            )
    });
}
