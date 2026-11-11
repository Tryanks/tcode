//! Settings → Traverse: the relay and lookup services this machine publishes
//! to, each with how it is doing while hosting.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::settings::{TraverseInstance, TraverseSetting, TraverseSource};
use tcode_protocol::{
    TraverseLookupState, TraverseManifestState, TraverseRelayStatus, TraverseSourceStatus,
};

use super::hosting::{
    RemoteController, apply_hosting_settings, custom_traverse_url, field_labels,
    reflowing_field_row,
};
use crate::icon::{Icon, IconName};
use crate::material::group_caption;
use crate::overlay::{Notification, OverlayExt as _};
use crate::settings_rows::{ToggleRow, footnote};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::switch::Switch;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// One Traverse source as the page edits it.
struct TraverseRow {
    enabled: bool,
    /// A self-hosted instance's base URL as typed; `None` is the official
    /// service.
    url: Option<(Entity<InputState>, gpui::Subscription)>,
    /// Whether every relay and lookup of its status is shown.
    expanded: bool,
}

pub struct TraversePanel {
    /// The sources in order, the official service first.
    rows: Vec<TraverseRow>,
    /// The saved setting the rows last took up or saved.
    saved: TraverseSetting,
    /// The URL of a self-hosted instance about to be added.
    add_input: Entity<InputState>,
    /// Repaints the status as it changes while hosting.
    status_events: Option<Task<()>>,
    /// The panel's size-class motion: the window draws it under another
    /// parent when it turns compact or wide.
    motion: crate::motion::owned::Owned,
    _subscriptions: [gpui::Subscription; 2],
}

impl TraversePanel {
    /// Built only where this process hosts: the page edits the
    /// [`RemoteController`]'s Traverse setting, and follows it when another
    /// host's settings page saves it.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = cx
            .global::<RemoteController>()
            .local_settings()
            .traverse
            .clone();
        let rows = rows_for(&saved, window, cx);
        let add_input = cx.new(|cx| InputState::new(window, cx));
        // Enter adds the typed instance; the field repaints as the URL turns
        // valid.
        let subscription = cx.subscribe_in(
            &add_input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.add(window, cx),
                InputEvent::Change => cx.notify(),
                InputEvent::Blur | InputEvent::Focus => {}
            },
        );
        let follow = cx.observe_global_in::<RemoteController>(window, |this, window, cx| {
            let saved = cx
                .global::<RemoteController>()
                .local_settings()
                .traverse
                .clone();
            if saved != this.saved {
                this.rows = rows_for(&saved, window, cx);
                this.saved = saved;
                cx.notify();
            }
        });
        Self {
            rows,
            saved,
            add_input,
            status_events: None,
            motion: Default::default(),
            _subscriptions: [subscription, follow],
        }
    }

    fn follow_status(&mut self, hosting: bool, cx: &mut Context<Self>) {
        match (hosting, self.status_events.is_some()) {
            (true, false) => {
                let Some(events) = cx.global::<RemoteController>().traverse_events() else {
                    return;
                };
                self.status_events = Some(cx.spawn(async move |this, cx| {
                    while events.recv().await.is_ok() {
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            return;
                        }
                    }
                    // Hosting stopped: a later start is followed afresh.
                    let _ = this.update(cx, |this, cx| {
                        this.status_events = None;
                        cx.notify();
                    });
                }));
            }
            (false, true) => self.status_events = None,
            _ => {}
        }
    }

    /// The self-hosted URLs as typed, one per row (`None` for the official
    /// service), each a usable base URL or why it is not one.
    fn typed_urls(&self, cx: &App) -> Vec<Option<Result<url::Url, SharedString>>> {
        let mut seen = Vec::new();
        self.rows
            .iter()
            .map(|row| {
                let (input, _) = row.url.as_ref()?;
                Some(
                    match custom_traverse_url(&input.read(cx).value()) {
                        None => Err(crate::tr!("remote.traverse.invalid_url")),
                        Some(url) if seen.contains(&url) => {
                            Err(crate::tr!("remote.traverse.duplicate"))
                        }
                        Some(url) => {
                            seen.push(url.clone());
                            Ok(url)
                        }
                    }
                    .map_err(|error| error.into_owned().into()),
                )
            })
            .collect()
    }

    /// The setting as edited, or `None` while a self-hosted URL is not a
    /// usable one.
    fn typed(&self, cx: &App) -> Option<TraverseSetting> {
        let sources = self
            .rows
            .iter()
            .zip(self.typed_urls(cx))
            .map(|(row, url)| {
                Some(TraverseSource {
                    instance: match url {
                        None => TraverseInstance::Official,
                        Some(url) => TraverseInstance::Custom {
                            url: url.ok()?.to_string(),
                        },
                    },
                    enabled: row.enabled,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(TraverseSetting::new(sources))
    }

    /// The instance typed into the add field, or why it cannot be added;
    /// `None` while the field is empty.
    fn typed_addition(&self, cx: &App) -> Option<Result<url::Url, SharedString>> {
        let value = self.add_input.read(cx).value();
        if value.trim().is_empty() {
            return None;
        }
        Some(match custom_traverse_url(&value) {
            None => Err(crate::tr!("remote.traverse.invalid_url")
                .into_owned()
                .into()),
            Some(url)
                if self
                    .typed_urls(cx)
                    .into_iter()
                    .flatten()
                    .any(|typed| typed.as_ref() == Ok(&url)) =>
            {
                Err(crate::tr!("remote.traverse.duplicate").into_owned().into())
            }
            Some(url) => Ok(url),
        })
    }

    /// Save the edits, re-binding the endpoint while hosting so they take
    /// effect at once. Nothing happens while they match what is saved or are
    /// incomplete, so leaving a field untouched never restarts the host.
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let controller = cx.global::<RemoteController>();
        let name = controller.local_settings().remote_host_name.clone();
        match self.typed(cx) {
            Some(traverse) if traverse != controller.local_settings().traverse => {
                self.saved = traverse.clone();
                // The hosting switch that shows a failure is on another page.
                if let Err(error) = apply_hosting_settings(traverse, name, cx) {
                    window.push_notification(Notification::error(error), cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// A source's switch applies at once.
    fn set_enabled(
        &mut self,
        index: usize,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows.get_mut(index) {
            row.enabled = enabled;
        }
        self.apply(window, cx);
    }

    /// Remove a self-hosted instance; the official service is only ever
    /// switched off.
    fn remove(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.get(index).is_some_and(|row| row.url.is_some()) {
            self.rows.remove(index);
        }
        self.apply(window, cx);
    }

    /// Add the typed instance, enabled, and apply it at once.
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(url)) = self.typed_addition(cx) else {
            return;
        };
        let input = url_input(url.as_str(), window, cx);
        self.rows.push(TraverseRow {
            enabled: true,
            expanded: false,
            url: Some(input),
        });
        self.add_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.apply(window, cx);
    }

    fn toggle_detail(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.expanded = !row.expanded;
        }
        cx.notify();
    }

    fn render_rows(
        &mut self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let motion = self.motion.clone();
        let urls = self.typed_urls(cx);
        let controller = cx.global::<RemoteController>();
        let (hosting, statuses) = (controller.is_hosting(), controller.traverse_status());
        self.follow_status(hosting, cx);
        // A source keeps its key for as long as it is listed, whatever its
        // place: a removed one folds away and the ones below move up.
        let sources: Vec<_> = self
            .rows
            .iter()
            .zip(urls)
            .enumerate()
            .map(|(index, (source, url))| {
                let input = source.url.as_ref().map(|(input, _)| input.clone());
                let key = match &input {
                    None => SharedString::from("official"),
                    Some(input) => format!("custom-{}", input.entity_id()).into(),
                };
                (key, (index, source.enabled, source.expanded, input, url))
            })
            .collect();
        let sources = crate::settings_rows::grouped_keyed(
            "remote-traverse-sources",
            sources,
            window,
            cx,
            |key, (index, enabled, expanded, input, url), window, cx| {
                let (index, enabled, expanded) = (*index, *enabled, *expanded);
                // The status names a source as invitations do.
                let status_key = match &url {
                    None => Some(tcode_client::pairing::TRAVERSE_OFFICIAL.to_owned()),
                    Some(Ok(url)) => Some(url.to_string()),
                    Some(Err(_)) => None,
                };
                let status = status_key.filter(|_| hosting).and_then(|status_key| {
                    statuses.iter().find(|status| status.source == status_key)
                });
                let chip = crate::motion::shown(
                    SharedString::from(format!("remote-traverse-chip-{key}")),
                    enabled.then_some(()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|shown| shown.fade(status_chip(index, key, status, expanded, window, cx)));
                let detail = status.filter(|_| enabled).map(|status| {
                    div()
                        .w_full()
                        .px(px(crate::material::CARD_INSET))
                        .pb(cx.theme().tokens.spacing.sm)
                        .child(
                            crate::widgets::Collapsible::new(
                                ("remote-traverse-detail", index),
                                expanded,
                            )
                            .motion(&self.motion)
                            .w_full()
                            .content(status_detail(key, status, window, cx)),
                        )
                });
                let switch_id = SharedString::from(format!("remote-traverse-{index}"));
                let set_enabled = cx.listener(move |this, checked: &bool, window, cx| {
                    this.set_enabled(index, *checked, window, cx);
                });
                let element = match (input, url) {
                    (Some(input), Some(url)) => {
                        let description = match url {
                            Ok(_) => crate::tr!("remote.traverse.url_description")
                                .into_owned()
                                .into(),
                            Err(error) => error.clone(),
                        };
                        let remove = Button::new(SharedString::from(format!(
                            "remote-traverse-remove-{index}"
                        )))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .aria_label(crate::tr!("remote.traverse.remove"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.remove(index, window, cx);
                            },
                        ));
                        let labels = field_labels(
                            crate::tr!("remote.traverse.custom").into_owned().into(),
                            Some(description),
                            compact,
                            cx,
                        );
                        let id = SharedString::from(format!("remote-traverse-field-{key}"));
                        // Beside a text field the row cannot be one
                        // control, so the switch is its own.
                        let rest = h_flex()
                            .gap_3()
                            .items_center()
                            .child(remove)
                            .children(chip)
                            .child(
                                Switch::new(switch_id)
                                    .checked(enabled)
                                    .on_click(set_enabled),
                            );
                        let input = input.clone();
                        reflowing_field_row(
                            id.clone(),
                            labels,
                            |row, cx| {
                                url_controls(
                                    row,
                                    &motion,
                                    &id,
                                    Input::new(&input)
                                        .placeholder(crate::tr!("remote.traverse.url_placeholder")),
                                    rest,
                                    cx,
                                )
                            },
                            compact,
                            &motion,
                            window,
                            cx,
                        )
                    }
                    _ => ToggleRow::new(
                        switch_id,
                        crate::tr!("remote.traverse.official"),
                        enabled,
                        compact,
                        set_enabled,
                    )
                    .accessory(chip.map(IntoElement::into_any_element))
                    .into_any_element(),
                };
                v_flex()
                    .w_full()
                    .child(element)
                    .children(detail)
                    .into_any_element()
            },
        );
        let mut rows = vec![sources];
        let addition = self.typed_addition(cx);
        let add_description = match &addition {
            Some(Err(error)) => error.clone(),
            _ => crate::tr!("remote.traverse.url_description")
                .into_owned()
                .into(),
        };
        let add = Button::new("remote-traverse-add")
            .ghost()
            .outline()
            .compact()
            .icon(IconName::Plus)
            .label(crate::tr!("remote.traverse.add"))
            .disabled(!matches!(addition, Some(Ok(_))))
            .on_click(cx.listener(|this, _, window, cx| {
                this.add(window, cx);
            }));
        let labels = field_labels(
            crate::tr!("remote.traverse.add_title").into_owned().into(),
            Some(add_description),
            compact,
            cx,
        );
        rows.push(crate::settings_rows::hairlined(
            reflowing_field_row(
                "remote-traverse-add-row",
                labels,
                |row, cx| {
                    url_controls(
                        row,
                        &self.motion,
                        "remote-traverse-add-row",
                        Input::new(&self.add_input)
                            .placeholder(crate::tr!("remote.traverse.url_placeholder")),
                        h_flex().items_center().child(add),
                        cx,
                    )
                },
                compact,
                &self.motion,
                window,
                cx,
            ),
            cx,
        ));
        rows
    }
}

/// A URL field followed by `rest`, the row's other controls: [`FIELD_WIDTH`]
/// beside the labels, and what `rest` leaves of the row under them, its
/// width gliding between the two when the window turns.
fn url_controls(
    row: &crate::settings_rows::FlipRow,
    motion: &crate::motion::owned::Owned,
    id: &str,
    field: impl IntoElement,
    rest: gpui::Div,
    cx: &App,
) -> AnyElement {
    let gap = cx.theme().tokens.spacing.md;
    let rest_width =
        motion.keep::<std::rc::Rc<std::cell::Cell<gpui::Pixels>>>(format!("{id}-rest"));
    let field = row
        .field_among(field, rest_width.get() + gap, cx)
        .min_w_0()
        .flex_shrink(1.);
    let rest = gpui_base::ElementExt::on_prepaint(rest, move |bounds, _, _| {
        rest_width.set(bounds.size.width)
    });
    h_flex()
        .min_w_0()
        .gap(gap)
        .items_center()
        .when(row.compact(), |controls| controls.w_full())
        .child(field)
        .child(rest)
        .into_any_element()
}

/// The field holding a self-hosted instance's URL, which takes effect when it
/// is left or Enter is pressed.
/// One row per saved source.
fn rows_for(
    traverse: &TraverseSetting,
    window: &mut Window,
    cx: &mut Context<TraversePanel>,
) -> Vec<TraverseRow> {
    traverse
        .sources
        .iter()
        .map(|source| TraverseRow {
            enabled: source.enabled,
            expanded: false,
            url: match &source.instance {
                TraverseInstance::Official => None,
                TraverseInstance::Custom { url } => Some(url_input(url, window, cx)),
            },
        })
        .collect()
}

fn url_input(
    url: &str,
    window: &mut Window,
    cx: &mut Context<TraversePanel>,
) -> (Entity<InputState>, gpui::Subscription) {
    let input = cx.new(|cx| InputState::new(window, cx).default_value(url.to_owned()));
    let subscription = cx.subscribe_in(
        &input,
        window,
        |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::Blur | InputEvent::PressEnter { .. } => this.apply(window, cx),
            InputEvent::Change => cx.notify(),
            InputEvent::Focus => {}
        },
    );
    (input, subscription)
}

fn manifest_word(state: TraverseManifestState) -> SharedString {
    match state {
        TraverseManifestState::Live => crate::tr!("remote.traverse.status.live"),
        TraverseManifestState::Cached => crate::tr!("remote.traverse.status.cached"),
        TraverseManifestState::Bundled => crate::tr!("remote.traverse.status.bundled"),
        TraverseManifestState::Failed => crate::tr!("remote.traverse.status.failed"),
    }
    .into_owned()
    .into()
}

/// A source's state at a glance: a fetched manifest is current, a cached or
/// bundled one may be stale, a source without one offers nothing, and one
/// not checked yet (or while not hosting) has no state.
fn chip_style(state: Option<TraverseManifestState>, cx: &App) -> (SharedString, gpui::Hsla) {
    let (label, tone) = match state {
        Some(TraverseManifestState::Live) => {
            (crate::tr!("remote.traverse.chip.live"), cx.theme().success)
        }
        Some(TraverseManifestState::Cached) => (
            crate::tr!("remote.traverse.chip.cached"),
            cx.theme().warning,
        ),
        Some(TraverseManifestState::Bundled) => (
            crate::tr!("remote.traverse.chip.bundled"),
            cx.theme().warning,
        ),
        Some(TraverseManifestState::Failed) => {
            (crate::tr!("remote.traverse.chip.failed"), cx.theme().danger)
        }
        None => (
            crate::tr!("remote.traverse.chip.unchecked"),
            cx.theme().muted_foreground,
        ),
    };
    (label.into_owned().into(), tone)
}

/// The status of an enabled source's row: its state chip, and with a status
/// to show, the disclosure that opens every relay and lookup under the row.
fn status_chip(
    index: usize,
    key: &SharedString,
    status: Option<&TraverseSourceStatus>,
    expanded: bool,
    window: &mut Window,
    cx: &mut Context<TraversePanel>,
) -> AnyElement {
    let (label, tone) = chip_style(status.map(|status| status.manifest.state), cx);
    // A new state cross-fades from the chip that showed the last one.
    let chip = crate::motion::cross_fade(
        SharedString::from(format!("remote-traverse-state-{key}")),
        (label.clone(), tone),
        CHANGE,
        window,
        cx,
        |(label, tone), _, cx| {
            crate::material::semantic_chip(label.clone(), tone.opacity(0.15), *tone, cx)
                .flex_none()
                .into_any_element()
        },
    );
    // Once a status arrives the chip becomes a disclosure: its chevron
    // widens in beside it.
    let disclosure = status.is_some();
    let turned = gpui_base::motion::transition(
        SharedString::from(format!("remote-traverse-chevron-{key}")),
        if expanded { 1. } else { 0. },
        gpui_base::motion::Transition::new(GLIDE),
        window,
        cx,
    );
    let chevron = crate::motion::shown(
        SharedString::from(format!("remote-traverse-chevron-shown-{key}")),
        disclosure.then_some(()),
        SHOW,
        window,
        cx,
    )
    .map(|shown| {
        shown.fold_across_gap(
            px(4.),
            Icon::new(IconName::ChevronDown).rotate(gpui::radians(std::f32::consts::PI * turned)),
        )
    });
    let id = SharedString::from(format!("remote-traverse-status-{index}"));
    let muted = cx.theme().muted_foreground;
    let selector = move || format!("remote-traverse-status-{index}");
    if disclosure {
        crate::material::accessible_clickable(h_flex(), id, gpui::Role::Button, label, cx)
            .aria_expanded(expanded)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                // The official source's row is its switch; expanding the
                // status must not also flip it.
                cx.stop_propagation();
                this.toggle_detail(index, cx);
            }))
            .debug_selector(selector)
            .flex_none()
            .items_center()
            .text_color(muted)
            .child(chip)
            .children(chevron)
            .into_any_element()
    } else {
        h_flex()
            .id(id)
            .debug_selector(selector)
            .flex_none()
            .items_center()
            .text_color(muted)
            .child(chip)
            .children(chevron)
            .into_any_element()
    }
}

/// The region the manifest gives a relay, else what tells its URL apart.
fn relay_name(relay: &TraverseRelayStatus) -> String {
    relay
        .region
        .clone()
        .or_else(|| super::relay_region(&relay.url))
        .unwrap_or_else(|| relay.url.clone())
}

/// Every relay with its latency, and every lookup with its last check.
fn status_detail(
    source: &SharedString,
    status: &TraverseSourceStatus,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let danger = cx.theme().danger;
    let id = move |part: &str, key: &str| {
        SharedString::from(format!("remote-traverse-{source}-{part}-{key}"))
    };
    let heading = |text: SharedString, cx: &App| {
        div()
            .pt(cx.theme().tokens.spacing.xs)
            .text_caption(cx)
            .child(text)
    };
    let line = |name: String, value: AnyElement| {
        h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(name)),
            )
            .child(value)
    };
    // A failure folds in under its line when a poll reports one, and out
    // once one clears it. It rides with its line and carries its own gap,
    // so it takes that gap with it.
    let gap = cx.theme().tokens.spacing.sm;
    let error = |id: SharedString, text: Option<&str>, window: &mut Window, cx: &mut App| {
        crate::motion::shown(id, text.map(str::to_owned), SHOW, window, cx).map(|shown| {
            shown.fold(
                div()
                    .w_full()
                    .pt(gap)
                    .text_color(danger)
                    .child(shown.value.clone()),
            )
        })
    };
    let with_error =
        |line: gpui::Div, error: Option<AnyElement>| v_flex().w_full().child(line).children(error);
    let manifest = &status.manifest;
    let mut manifest_value = vec![manifest_word(manifest.state).to_string()];
    if let Some(fetched) = manifest.fetched_unix {
        manifest_value.push(
            crate::tr!(
                "remote.traverse.status.fetched",
                time = crate::time::humanize_ago(crate::time::now_secs(cx).saturating_sub(fetched))
            )
            .into_owned(),
        );
    }
    let manifest_error = error(
        id("manifest", "error"),
        manifest.error.as_deref(),
        window,
        cx,
    );
    // A relay or a lookup the service adds or drops folds in or out where
    // it is; each carries the gap above it, and a heading folds with the
    // first of its kind and the last.
    let column = crate::material::card(cx)
        .w_full()
        .gap_0()
        .text_token(&cx.theme().tokens.typography.xs)
        .child(with_error(
            line(
                crate::tr!("remote.traverse.status.manifest").into_owned(),
                div()
                    .flex_none()
                    .text_color(muted)
                    .child(manifest_value.join(" · "))
                    .into_any_element(),
            ),
            manifest_error,
        ));
    let titled = |key: &str, title: Option<SharedString>, window: &mut Window, cx: &mut App| {
        crate::motion::shown(id(key, "heading"), title, SHOW, window, cx)
            .map(|shown| shown.fold(div().pt(gap).child(heading(shown.value.clone(), cx))))
    };
    let relays_title = titled(
        "relays",
        (!status.relays.is_empty()).then(|| {
            crate::tr!("remote.traverse.status.relays_title")
                .into_owned()
                .into()
        }),
        window,
        cx,
    );
    let relays = status
        .relays
        .iter()
        .map(|relay| (SharedString::from(relay.url.clone()), relay.clone()));
    let relays = crate::motion::keyed(
        id("relays", "rows"),
        relays,
        ROWS,
        window,
        cx,
        |_, relay, window, cx| {
            let latency = match relay.latency_ms {
                Some(ms) => crate::tr!("remote.traverse.status.latency", ms = ms),
                None => crate::tr!("remote.traverse.status.unmeasured"),
            };
            let home = crate::motion::shown(
                id("home", &relay.url),
                relay.home.then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                shown.fade(crate::material::semantic_chip(
                    crate::tr!("remote.traverse.status.home").into_owned(),
                    cx.theme().success.opacity(0.15),
                    cx.theme().success,
                    cx,
                ))
            });
            let relay_error = error(id("relay", &relay.url), relay.error.as_deref(), window, cx);
            div()
                .w_full()
                .pt(gap)
                .child(with_error(
                    line(
                        relay_name(relay),
                        h_flex()
                            .flex_none()
                            .gap_2()
                            .items_center()
                            .children(home)
                            .child(div().text_color(muted).child(latency.into_owned()))
                            .into_any_element(),
                    ),
                    relay_error,
                ))
                .into_any_element()
        },
    );
    let lookups_title = titled(
        "lookups",
        (!status.lookups.is_empty()).then(|| {
            crate::tr!("remote.traverse.status.lookups_title")
                .into_owned()
                .into()
        }),
        window,
        cx,
    );
    let lookups = status
        .lookups
        .iter()
        .map(|lookup| (SharedString::from(lookup.url.clone()), lookup.clone()));
    let lookups = crate::motion::keyed(
        id("lookups", "rows"),
        lookups,
        ROWS,
        window,
        cx,
        |_, lookup, window, cx| {
            let name = url::Url::parse(&lookup.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| lookup.url.clone());
            let (value, color) = match lookup.state {
                TraverseLookupState::Ok => (crate::tr!("remote.traverse.status.ok"), muted),
                TraverseLookupState::Pending => {
                    (crate::tr!("remote.traverse.status.pending"), muted)
                }
                TraverseLookupState::Failed => {
                    (crate::tr!("remote.traverse.status.failed"), danger)
                }
            };
            let value = crate::motion::cross_fade(
                id("lookup", &lookup.url),
                (SharedString::from(value.into_owned()), color),
                CHANGE,
                window,
                cx,
                |(value, color), _, _| {
                    div()
                        .flex_none()
                        .text_color(*color)
                        .child(value.clone())
                        .into_any_element()
                },
            );
            let lookup_error = error(
                id("lookup-error", &lookup.url),
                lookup.error.as_deref(),
                window,
                cx,
            );
            div()
                .w_full()
                .pt(gap)
                .child(with_error(line(name, value), lookup_error))
                .into_any_element()
        },
    );
    let column = column
        .children(relays_title)
        .child(relays.render(v_flex().w_full()))
        .children(lookups_title)
        .child(lookups.render(v_flex().w_full()));
    column.into_any_element()
}

impl Render for TraversePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let rows = self.render_rows(compact, window, cx);
        v_flex()
            .w_full()
            .min_w_0()
            .child(group_caption(crate::tr!("remote.traverse.section"), cx))
            .child(crate::material::group(cx).children(rows))
            .child(footnote(
                crate::tr!("remote.traverse.description"),
                compact,
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use tcode_client::HostLink;
    use tcode_core::settings::Settings;
    use tcode_traverse::HostMux;

    /// A source's switch, addition or removal takes effect when it is made:
    /// it is saved at once, and a self-hosted instance joins only once its
    /// URL is usable and not already listed.
    #[gpui::test]
    fn traverse_edits_apply_themselves(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("traverse-edits");
        std::fs::create_dir_all(&root).unwrap();
        // Idle pipes: the host is never attached to here.
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let (attachments, _attachment_rx) = async_channel::unbounded();
        cx.update(crate::test_client::init);
        cx.update(|cx| {
            cx.set_global(RemoteController::new(
                HostMux::new(attachments, from_host.clone()),
                root.clone(),
                None,
                HostLink::new(to_host, from_host),
                Settings::default(),
            ))
        });
        let (panel, cx) = cx.add_window_view(TraversePanel::new);
        let saved = |cx: &mut gpui::VisualTestContext| {
            cx.read(|cx| {
                serde_json::to_value(&cx.global::<RemoteController>().local_settings().traverse)
                    .unwrap()
            })
        };
        panel.update_in(cx, |panel, window, cx| {
            panel.set_enabled(0, false, window, cx)
        });
        assert_eq!(
            saved(cx),
            serde_json::json!({"sources": [{"kind": "official", "enabled": false}]})
        );
        for typed in [
            "traverse.example",
            "https://traverse.example",
            "https://traverse.example/",
        ] {
            panel.update_in(cx, |panel, window, cx| {
                panel
                    .add_input
                    .update(cx, |input, cx| input.set_value(typed, window, cx));
                panel.add(window, cx);
            });
        }
        assert_eq!(
            saved(cx),
            serde_json::json!({"sources": [
                {"kind": "official", "enabled": false},
                {"kind": "custom", "url": "https://traverse.example/", "enabled": true},
            ]})
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.set_enabled(0, true, window, cx);
            panel.remove(1, window, cx);
        });
        assert_eq!(
            saved(cx),
            serde_json::json!({"sources": [{"kind": "official", "enabled": true}]})
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
