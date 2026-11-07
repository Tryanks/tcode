//! The environment-variable rows of a harness, an account or an ACP agent.
//! Each row commits as it is left: a name or value on blur or Enter, a
//! sensitivity switch or a removal at once. Sensitive values go to the host's
//! secret store and never come back; a stored one shows as a placeholder.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};

use crate::icon::IconName;
use crate::settings::EnvVar;
use crate::sizing::Sizable as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::switch::Switch;

/// How long a row drawn here takes to fold in or out.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// What a commit hands its owner: the rows to save, then each secret to
/// store (`Some`) or clear (`None`), by name.
pub(crate) type CommitEnv = Rc<dyn Fn(Vec<EnvVar>, Vec<(String, Option<String>)>, &mut App)>;

struct Row {
    key: u64,
    name: Entity<InputState>,
    value: Entity<InputState>,
    sensitive: bool,
    /// A sensitive row whose value is in the secret store.
    stored: bool,
    _subscriptions: [Subscription; 2],
}

pub(crate) struct EnvEditor {
    id: SharedString,
    rows: Vec<Row>,
    next_key: u64,
    /// The sensitive names whose secrets are stored, as of the last commit.
    stored: HashSet<String>,
    commit: CommitEnv,
}

impl EnvEditor {
    pub(crate) fn new(
        id: impl Into<SharedString>,
        env: &[EnvVar],
        stored: HashSet<String>,
        commit: CommitEnv,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self {
            id: id.into(),
            rows: Vec::new(),
            next_key: 0,
            stored: HashSet::new(),
            commit,
        };
        editor.seed(env, stored, window, cx);
        editor
    }

    /// Show `env` as saved, dropping whatever is being edited.
    pub(crate) fn seed(
        &mut self,
        env: &[EnvVar],
        stored: HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rows = env
            .iter()
            .map(|var| {
                let value = if var.sensitive {
                    String::new()
                } else {
                    var.value.clone()
                };
                let stored = var.sensitive && stored.contains(&var.name);
                self.row(&var.name, &value, var.sensitive, stored, window, cx)
            })
            .collect();
        self.stored = stored;
        cx.notify();
    }

    /// The rows as saved, for telling whether `env` is what this shows.
    pub(crate) fn shows(&self, env: &[EnvVar], cx: &App) -> bool {
        self.rows.len() == env.len()
            && self.rows.iter().zip(env).all(|(row, var)| {
                row.name.read(cx).value().trim() == var.name
                    && row.sensitive == var.sensitive
                    && (var.sensitive || row.value.read(cx).value() == var.value)
            })
    }

    /// Whether one of the rows is being typed in.
    pub(crate) fn focused(&self, window: &Window, cx: &App) -> bool {
        self.rows.iter().any(|row| {
            row.name.read(cx).focus_handle(cx).is_focused(window)
                || row.value.read(cx).focus_handle(cx).is_focused(window)
        })
    }

    fn row(
        &mut self,
        name: &str,
        value: &str,
        sensitive: bool,
        stored: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Row {
        let input = |text: &str, masked: bool, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut input = InputState::new(window, cx).masked(masked);
                input.set_value(text.to_owned(), window, cx);
                input
            })
        };
        let name = input(name, false, window, cx);
        let value = input(value, sensitive, window, cx);
        self.next_key += 1;
        Row {
            key: self.next_key,
            _subscriptions: [cx.subscribe(&name, leave), cx.subscribe(&value, leave)],
            name,
            value,
            sensitive,
            stored,
        }
    }

    pub(crate) fn commit(&mut self, cx: &mut Context<Self>) {
        let mut env = Vec::new();
        let mut secrets = Vec::new();
        let mut sensitive = HashSet::new();
        for row in &mut self.rows {
            let name = row.name.read(cx).value().trim().to_string();
            if name.is_empty() {
                continue;
            }
            let value = row.value.read(cx).value().to_string();
            if row.sensitive {
                // An empty field keeps the stored secret as it is.
                if !value.is_empty() {
                    secrets.push((name.clone(), Some(value.clone())));
                    row.stored = true;
                }
                sensitive.insert(name.clone());
            }
            env.push(EnvVar {
                name,
                value: if row.sensitive { String::new() } else { value },
                sensitive: row.sensitive,
            });
        }
        // A secret whose row went, was renamed or turned plain is cleared.
        secrets.extend(
            self.stored
                .iter()
                .filter(|name| !sensitive.contains(*name))
                .map(|name| (name.clone(), None)),
        );
        self.stored = self
            .rows
            .iter()
            .filter(|row| row.sensitive && row.stored)
            .map(|row| row.name.read(cx).value().trim().to_string())
            .collect();
        (self.commit)(env, secrets, cx);
        cx.notify();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A new variable is sensitive until said otherwise.
        let row = self.row("", "", true, false, window, cx);
        let name = row.name.clone();
        self.rows.push(row);
        name.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn remove(&mut self, key: u64, cx: &mut Context<Self>) {
        self.rows.retain(|row| row.key != key);
        self.commit(cx);
    }

    fn toggle_sensitive(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.rows.iter().position(|row| row.key == key) else {
            return;
        };
        let row = &self.rows[index];
        let name = row.name.read(cx).value().to_string();
        let value = row.value.read(cx).value().to_string();
        let sensitive = !row.sensitive;
        let replaced = self.row(&name, &value, sensitive, false, window, cx);
        self.rows[index] = Row { key, ..replaced };
        self.commit(cx);
    }
}

/// A row's field was left: what it holds is saved.
fn leave(
    editor: &mut EnvEditor,
    _: Entity<InputState>,
    event: &InputEvent,
    cx: &mut Context<EnvEditor>,
) {
    if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
        editor.commit(cx);
    }
}

impl Render for EnvEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = self.id.clone();
        let rows: Vec<(SharedString, Option<u64>)> = if self.rows.is_empty() {
            vec![("env-empty".into(), None)]
        } else {
            self.rows
                .iter()
                .map(|row| {
                    (
                        SharedString::from(format!("env-{}", row.key)),
                        Some(row.key),
                    )
                })
                .collect()
        };
        let rows = crate::motion::keyed(
            SharedString::from(format!("{id}-rows")),
            rows,
            ROWS,
            window,
            cx,
            |_, key, _, cx| {
                let Some(key) = *key else {
                    return div()
                        .pt_2()
                        .text_secondary(false, cx)
                        .child(crate::tr!("providers.env.empty"))
                        .into_any_element();
                };
                let Some(row) = self.rows.iter().find(|row| row.key == key) else {
                    return div().into_any_element();
                };
                let value_placeholder = if row.stored {
                    crate::tr!("providers.env.stored_secret")
                } else {
                    crate::tr!("providers.env.value_placeholder")
                };
                h_flex()
                    .w_full()
                    .pt_2()
                    .gap_2()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&row.name)
                                .placeholder(crate::tr!("providers.env.name_placeholder")),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&row.value).placeholder(value_placeholder)),
                    )
                    .child(
                        Switch::new(SharedString::from(format!("{id}-sensitive-{key}")))
                            .checked(row.sensitive)
                            .tooltip(crate::tr!("providers.env.sensitive"))
                            .on_click(cx.listener(move |this, _: &bool, window, cx| {
                                this.toggle_sensitive(key, window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-remove-{key}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Delete)
                            .tooltip(crate::tr!("providers.env.remove"))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove(key, cx))),
                    )
                    .into_any_element()
            },
        );
        let any_sensitive = self.rows.iter().any(|row| row.sensitive);
        v_flex()
            .w_full()
            .child(rows.render(v_flex().w_full().mt(gpui::rems(-0.5))))
            .child(
                h_flex()
                    .w_full()
                    .pt_2()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .when(any_sensitive, |note| {
                                note.child(crate::tr!("providers.env.security_help"))
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-add")))
                            .debug_selector({
                                let id = id.clone();
                                move || format!("{id}-add")
                            })
                            .outline()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label(crate::tr!("providers.env.add"))
                            .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                    ),
            )
    }
}
