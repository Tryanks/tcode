//! Reusable native-provider model picker used by settings surfaces.
//!
//! The profile tabs (one per provider profile, built-in or user-created),
//! resolved model catalog, offline/custom-model handling, and selection event
//! live here so settings surfaces do not grow subtly different pickers. One
//! picker holds a single selection; another adds models to a list its owner
//! keeps, one pick after another.

use std::rc::Rc;

/// How long a model option takes to fold in or out as the list changes.
const OPTIONS: std::time::Duration = std::time::Duration::from_millis(200);

use crate::icon::{Icon, IconName};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::tabs::{TabBar, TabItem};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{PopoverState, h_flex, v_flex};

use agent::ProviderKind;
use tcode_core::control::AgentScope;
use tcode_core::settings::Settings;

use crate::provider_card::profile_glyph;
use crate::provider_status::{ProviderSnapshot, redact_email, signed_in_account};
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelOption {
    pub provider: ProviderKind,
    pub id: String,
    pub name: String,
    pub profile_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelSelected(pub ModelOption);

pub(crate) struct ProviderModelPicker {
    store: Entity<WorkspaceStore>,
    popover_id: &'static str,
    trigger_id: &'static str,
    /// The active tab: a profile id (built-in or user-created).
    selected_profile: String,
    selected: Option<(ProviderKind, String, Option<String>)>,
    /// What the trigger reads while nothing is chosen.
    unchosen: SharedString,
    /// The picker adds to a list its owner keeps: this names its trigger, a
    /// pick leaves it open, and it offers what the list does not hold yet.
    adding: Option<SharedString>,
    /// What the owner's list holds already.
    taken: Vec<(ProviderKind, String, Option<String>)>,
    /// Only what Settings → Control lets a thread be started on is offered.
    startable: bool,
    _store_subscription: Subscription,
}

impl EventEmitter<ModelSelected> for ProviderModelPicker {}

impl ProviderModelPicker {
    /// A picker showing `selected`, a provider, model and profile, or
    /// "Automatic" while nothing is chosen.
    pub fn selection(
        store: Entity<WorkspaceStore>,
        popover_id: &'static str,
        trigger_id: &'static str,
        selected: Option<(ProviderKind, String, Option<String>)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let store_subscription =
            observe_store_topics(&store, &[TopicKind::Settings, TopicKind::Providers], cx);
        Self {
            store,
            popover_id,
            trigger_id,
            selected_profile: selected
                .as_ref()
                .map(|(provider, _, profile_id)| {
                    selection_profile_id(*provider, profile_id.as_deref())
                })
                .unwrap_or_default(),
            selected,
            unchosen: crate::tr!("model_picker.automatic").into_owned().into(),
            adding: None,
            taken: Vec::new(),
            startable: false,
            _store_subscription: store_subscription,
        }
    }

    /// A picker that adds to a list its owner keeps: each pick is emitted and
    /// the picker stays open for the next. `label` names its trigger.
    pub fn adding(
        store: Entity<WorkspaceStore>,
        popover_id: &'static str,
        trigger_id: &'static str,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            adding: Some(label.into()),
            ..Self::selection(store, popover_id, trigger_id, None, cx)
        }
    }

    /// What the trigger reads while nothing is chosen, in place of "Automatic".
    pub fn unchosen(mut self, label: impl Into<SharedString>) -> Self {
        self.unchosen = label.into();
        self
    }

    /// Offer only what Settings → Control lets a thread be started on.
    pub fn startable(mut self) -> Self {
        self.startable = true;
        self
    }

    /// What the owner's list holds, which an adding picker leaves out.
    pub fn set_taken(
        &mut self,
        taken: Vec<(ProviderKind, String, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        if self.taken != taken {
            self.taken = taken;
            cx.notify();
        }
    }

    pub fn set_selected(
        &mut self,
        selected: Option<(ProviderKind, String, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        if self.selected != selected {
            if let Some((provider, _, profile_id)) = &selected {
                self.selected_profile = selection_profile_id(*provider, profile_id.as_deref());
            }
            self.selected = selected;
            cx.notify();
        }
    }

    /// The models a profile tab lists now: what it offers, less what the
    /// owner's list holds already.
    fn profile_options(&self, profile_id: &str, cx: &App) -> Vec<ModelOption> {
        let mut options = self.profile_offer(profile_id, cx);
        options.retain(|option| {
            !self.taken.iter().any(|(provider, model, profile_id)| {
                *provider == option.provider
                    && model == &option.id
                    && profile_id == &option.profile_id
            })
        });
        options
    }

    /// The models a profile tab offers. Only enabled profiles are offered for
    /// new selections; a disabled profile stays configurable in Settings but
    /// never reaches the picker.
    fn profile_offer(&self, profile_id: &str, cx: &App) -> Vec<ModelOption> {
        let store = self.store.read(cx);
        let Some(profile) = store
            .enabled_profiles()
            .into_iter()
            .find(|profile| profile.id == profile_id)
        else {
            return Vec::new();
        };
        let option_profile =
            (!Settings::is_builtin_profile_id(&profile.id)).then_some(profile.id.clone());
        let every = AgentScope::Every;
        let scope = if self.startable {
            &store.settings_ref().control.agents
        } else {
            &every
        };
        let Some(access) = scope.agent(&profile.id) else {
            return Vec::new();
        };
        store
            .picker_models_for_profile(&profile.id)
            .into_iter()
            .filter(|model| access.allows_model(Some(&model.id)))
            .map(|model| ModelOption {
                provider: profile.kind,
                id: model.id,
                name: model.name,
                profile_id: option_profile.clone(),
            })
            .collect()
    }

    pub(crate) fn display_name(
        &self,
        provider: ProviderKind,
        model: &str,
        profile_id: Option<&str>,
        cx: &App,
    ) -> String {
        self.profile_offer(&selection_profile_id(provider, profile_id), cx)
            .into_iter()
            .find(|option| {
                option.provider == provider
                    && option.id == model
                    && option.profile_id.as_deref() == profile_id
            })
            .map(|option| option.name)
            .unwrap_or_else(|| model.to_string())
    }

    fn trigger(&self, cx: &Context<Self>) -> Button {
        if let Some(label) = &self.adding {
            return Button::new(self.trigger_id)
                .ghost()
                .outline()
                .xsmall()
                .icon(IconName::Plus)
                .aria_label(label.clone());
        }
        let (display, glyph) = match &self.selected {
            Some((provider, model, profile_id)) => (
                self.display_name(*provider, model, profile_id.as_deref(), cx),
                Some(profile_glyph(
                    self.store.read(cx),
                    *provider,
                    profile_id.as_deref(),
                )),
            ),
            None => (self.unchosen.to_string(), None),
        };
        Button::new(self.trigger_id)
            .ghost()
            .outline()
            .compact()
            .child(
                h_flex()
                    .w(px(230.))
                    .items_center()
                    .gap_2()
                    .text_body(false, cx)
                    .children(glyph)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(crate::widgets::fade_text(display)),
                    )
                    .child(
                        Icon::new(IconName::ChevronDown).text_color(cx.theme().muted_foreground),
                    ),
            )
    }
}

impl Render for ProviderModelPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picker = cx.entity();
        let popover = crate::material::overlay_popover(self.popover_id, cx);
        // The plus sits at the right of its list's heading, so its models open
        // under it leftwards.
        let popover = if self.adding.is_some() {
            popover.anchor(gpui::Anchor::TopRight)
        } else {
            popover
        };
        popover
            .trigger(self.trigger(cx))
            .content(move |_, window, cx| {
                let (mut profiles, selected_profile, startable) = {
                    let picker = picker.read(cx);
                    (
                        picker.store.read(cx).enabled_profiles(),
                        picker.selected_profile.clone(),
                        picker.startable,
                    )
                };
                let store = picker.read(cx).store.clone();
                if startable {
                    // A profile Control leaves nothing of has no tab.
                    profiles.retain(|profile| {
                        !picker.read(cx).profile_offer(&profile.id, cx).is_empty()
                    });
                    if profiles.is_empty() {
                        return v_flex()
                            .w(crate::sizing::fit_viewport(
                                390.,
                                window.viewport_size().width,
                            ))
                            .p_1()
                            .child(crate::material::empty_row(
                                crate::tr!("model_picker.none_startable"),
                                cx,
                            ))
                            .into_any_element();
                    }
                }
                // A deleted profile falls back to the first tab (built-ins
                // always exist, so the list is never empty).
                let current_profile = if profiles.iter().any(|p| p.id == selected_profile) {
                    selected_profile
                } else {
                    profiles.first().map(|p| p.id.clone()).unwrap_or_default()
                };
                // The catalog is a viewport of its own; cap it against the
                // window so a short one scrolls instead of overflowing it.
                let catalog = crate::sizing::fit_viewport(300., window.viewport_size().height);
                // Each profile's options keep a layer of their own, so a tab's
                // list (and its scroll) fades out under the next one's.
                let current_tab = profiles
                    .iter()
                    .position(|profile| profile.id == current_profile)
                    .unwrap_or(0);
                let fade = crate::motion::CrossFade::new(
                    "settings-model-option-tabs",
                    current_tab,
                    profiles.len().max(1),
                    CHANGE,
                    window,
                    cx,
                );
                let popover = cx.entity();
                let layers: Vec<(usize, AnyElement)> = profiles
                    .iter()
                    .enumerate()
                    .filter(|(tab, _)| fade.shows(*tab))
                    .map(|(tab, profile)| {
                        let options: Rc<[ModelOption]> =
                            picker.read(cx).profile_options(&profile.id, cx).into();
                        let unlisted = Settings::is_builtin_profile_id(&profile.id)
                            .then(|| store.read(cx).models_error(profile.kind))
                            .flatten()
                            .map(str::to_owned);
                        let list = option_list(tab, options, unlisted, &picker, &popover, cx);
                        (tab, list)
                    })
                    .collect();
                let rows = fade.fill().render(layers).w_full().h(catalog);

                // Keep profile tabs in the same order as the settings cards.
                let mut tabs = TabBar::new("provider-model-picker-tabs")
                    .segmented()
                    .w_full();
                for (tab_index, profile) in profiles.iter().enumerate() {
                    let is_selected = profile.id == current_profile;
                    let label = store.read(cx).provider_profile_display_name(&profile.id);
                    let kind = profile.kind;
                    let profile_id = profile.id.clone();
                    let picker = picker.clone();
                    let popover = cx.entity();
                    tabs = tabs.item(
                        TabItem::new(("settings-provider-tab", tab_index), label)
                            .icon(profile_glyph(store.read(cx), kind, Some(&profile.id)))
                            .selected(is_selected)
                            .on_click(move |_, _, cx| {
                                picker.update(cx, |picker, cx| {
                                    picker.selected_profile = profile_id.clone();
                                    cx.notify();
                                });
                                popover.update(cx, |_, cx| cx.notify());
                            }),
                    );
                }

                let account = store
                    .read(cx)
                    .provider_profile_snapshot(&current_profile)
                    .as_ref()
                    .and_then(account_caption);

                v_flex()
                    .w(crate::sizing::fit_viewport(
                        390.,
                        window.viewport_size().width,
                    ))
                    .child(tabs)
                    .when_some(account, |this, account| {
                        this.child(
                            div()
                                .px_2()
                                .py_1()
                                .text_secondary(false, cx)
                                .min_w_0()
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(account)),
                        )
                    })
                    .child(crate::material::faded_hairline(cx))
                    .child(rows)
                    .when(startable, |this| {
                        this.child(crate::material::faded_hairline(cx)).child(
                            div()
                                .px_2()
                                .py_1p5()
                                .text_caption(cx)
                                .child(crate::tr!("model_picker.startable_only")),
                        )
                    })
                    .into_any_element()
            })
    }
}

/// One profile's options: an option the catalog adds or drops folds into or
/// out of the list as the others glide.
fn option_list(
    tab: usize,
    options: Rc<[ModelOption]>,
    // Why the profile's catalog could not be listed.
    unlisted: Option<String>,
    picker: &Entity<ProviderModelPicker>,
    popover: &Entity<PopoverState>,
    cx: &App,
) -> AnyElement {
    let (selected, adding) = {
        let picker = picker.read(cx);
        (picker.selected.clone(), picker.adding.is_some())
    };
    let available = options.clone();
    if available.is_empty() {
        return div()
            .size_full()
            .p_1()
            .child(crate::material::empty_row(
                match &unlisted {
                    Some(error) => crate::tr!("model_picker.models_unlisted", error = error),
                    None if adding => crate::tr!("model_picker.all_added"),
                    None => crate::tr!("model_picker.no_models"),
                },
                cx,
            ))
            .into_any_element();
    }
    let key = |option: &ModelOption| {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (option.provider, &option.id, &option.profile_id).hash(&mut hasher);
        hasher.finish()
    };
    let keys: Rc<[u64]> = available.iter().map(key).collect();
    let row = {
        let (selected, picker, popover) = (selected.clone(), picker.clone(), popover.clone());
        move |index: usize, option: &ModelOption, cx: &App| {
            let is_selected = selected
                .as_ref()
                .is_some_and(|(provider, model, profile_id)| {
                    *provider == option.provider
                        && model == &option.id
                        && profile_id == &option.profile_id
                });
            div()
                .pb_0p5()
                .child(option_row(
                    index,
                    option,
                    is_selected,
                    adding,
                    &picker,
                    &popover,
                    cx,
                ))
                .into_any_element()
        }
    };
    let leaving = {
        let row = row.clone();
        move |left: u64, _: &mut Window, cx: &mut App| {
            let index = options.iter().position(|option| key(option) == left)?;
            Some(row(index, &options[index], cx))
        }
    };
    crate::scroll::VirtualList::measured(
        ("settings-model-options", tab),
        available.len(),
        move |index, _, cx| row(index, &available[index], cx),
    )
    .keyed(keys, OPTIONS, leaving)
    .scrollbar()
    .size_full()
    .pt_1()
    .px_1()
    // The last row carries its gap.
    .pb(px(2.))
    .into_any_element()
}

fn option_row(
    index: usize,
    option: &ModelOption,
    is_selected: bool,
    adding: bool,
    picker: &Entity<ProviderModelPicker>,
    popover: &Entity<PopoverState>,
    cx: &App,
) -> AnyElement {
    let picker = picker.clone();
    let popover = popover.clone();
    let selected = option.clone();
    crate::material::accessible_clickable(
        crate::material::menu_item_frame(false, crate::material::ItemFill::Hover, cx),
        ("settings-model-option", index),
        gpui::Role::MenuItem,
        option.name.clone(),
        cx,
    )
    .debug_selector(|| format!("settings-model-option-{index}"))
    .aria_selected(is_selected)
    .flex_none()
    .w_full()
    .items_center()
    .text_body(false, cx)
    .cursor_pointer()
    .child(profile_glyph(
        picker.read(cx).store.read(cx),
        option.provider,
        option.profile_id.as_deref(),
    ))
    .child(
        v_flex()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(option.name.clone())),
            )
            .child(
                div()
                    .overflow_hidden()
                    .text_meta_mono(false, cx)
                    .child(crate::widgets::fade_text(option.id.clone())),
            ),
    )
    .when(is_selected, |row| row.child(Icon::new(IconName::Check)))
    .when(adding, |row| {
        row.child(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
    })
    .on_click(move |_, window, cx| {
        picker.update(cx, |picker, cx| {
            picker.selected_profile = option_profile_id(&selected);
            if !adding {
                picker.selected = Some((
                    selected.provider,
                    selected.id.clone(),
                    selected.profile_id.clone(),
                ));
            }
            cx.emit(ModelSelected(selected.clone()));
        });
        // An adding picker stays open for the next pick, and draws again
        // without the one its owner just took.
        popover.update(cx, |state, cx| {
            if adding {
                cx.notify();
            } else {
                state.dismiss(window, cx);
            }
        });
    })
    .into_any_element()
}

/// The account a profile tab is signed in to: its redacted email, then its
/// plan when the provider names one.
fn account_caption(snapshot: &ProviderSnapshot) -> Option<String> {
    let auth = signed_in_account(snapshot)?;
    let email = redact_email(auth.email.as_deref()?.trim());
    Some(
        match auth
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
        {
            Some(label) => format!("{email} · {label}"),
            None => email,
        },
    )
}

/// The profile a selection belongs to: its explicit profile id, or the kind's
/// built-in profile.
fn selection_profile_id(provider: ProviderKind, profile_id: Option<&str>) -> String {
    profile_id
        .map(str::to_string)
        .unwrap_or_else(|| Settings::builtin_profile_id(provider).to_string())
}

/// The profile tab an option files under (see [`selection_profile_id`]).
fn option_profile_id(option: &ModelOption) -> String {
    selection_profile_id(option.provider, option.profile_id.as_deref())
}
