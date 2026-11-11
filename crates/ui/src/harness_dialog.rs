//! A harness's settings (Settings → Harnesses → a harness): a dialog on a
//! wide window, a page of the settings stack on a compact one.
//!
//! What it shows comes from the host's description of the harness and from
//! its profiles, never from which harness it is: the accounts and endpoint
//! profiles it has or can add, the native fields it runs with, its switches,
//! what it integrates with. Every field commits as it is left; nothing waits
//! for a Save.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, ElementId, Entity, EventEmitter,
    Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_base::motion::transition;
use gpui_base::{h_flex, v_flex};

use agent::ProviderKind;
use tcode_core::provider_status::{AuthStatus, ProviderProbeDiagnostic, ProviderStatusKind};
use tcode_core::settings::{ProfileSettingsPatch, ResolvedProfile, Settings};
use tcode_protocol::HarnessDescription;

use crate::env_editor::{CommitEnv, EnvEditor};
use crate::icon::{Icon, IconName};
use crate::overlay::{DialogButtons, OverlayExt as _};
use crate::provider_models::{
    ResolvedModel, model_capability_label, slug_error_message, validate_slug,
};
use crate::provider_status::{ProviderSnapshot, redact_email, signed_in_account};
use crate::scroll::ScrollableElement as _;
use crate::settings::{ACCENT_PRESETS, EnvVar, provider_label};
use crate::settings_rows::{FIELD_WIDTH, ToggleRow, row_labels};
use crate::sizing::Sizable as _;
use crate::store::{StoreChange, TopicKind, WorkspaceStore};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::TooltipTrigger as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::switch::Switch;
use crate::widgets::tabs::{TabBar, TabItem};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long one page of the dialog takes to cross-fade to another.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// How many of an account's models show before the rest fold away.
const MODELS_SHOWN: usize = 5;

/// Which harness a dialog or row is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum HarnessId {
    Native(ProviderKind),
    Acp(String),
}

impl HarnessId {
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Native(kind) => tcode_core::settings::provider_key(*kind).to_owned(),
            Self::Acp(id) => format!("acp-{id}"),
        }
    }

    pub(crate) fn kind(&self) -> ProviderKind {
        match self {
            Self::Native(kind) => *kind,
            Self::Acp(_) => ProviderKind::Acp,
        }
    }

    pub(crate) fn of(description: &HarnessDescription) -> Self {
        match &description.acp_agent_id {
            Some(id) => Self::Acp(id.clone()),
            None => Self::Native(description.provider),
        }
    }
}

/// The host's description of `harness`, once the host has sent one.
pub(crate) fn describe(store: &WorkspaceStore, harness: &HarnessId) -> Option<HarnessDescription> {
    store
        .harness_descriptions()
        .iter()
        .find(|description| HarnessId::of(description) == *harness)
        .cloned()
}

/// A harness's profiles, its built-in one first.
pub(crate) fn harness_profiles(
    store: &WorkspaceStore,
    harness: &HarnessId,
) -> Vec<ResolvedProfile> {
    match harness {
        HarnessId::Native(kind) => store
            .all_provider_profiles()
            .into_iter()
            .filter(|profile| profile.kind == *kind)
            .collect(),
        HarnessId::Acp(_) => Vec::new(),
    }
}

/// Whether the harness lists its accounts and endpoint profiles, each with a
/// page of its own, rather than showing its one account on its own page.
pub(crate) fn lists_accounts(description: &HarnessDescription, profiles: usize) -> bool {
    description.acp_agent_id.is_none()
        && (profiles > 1 || description.account_login || description.offers_endpoint_profiles())
}

/// The harness's name, as rows and titles show it.
pub(crate) fn harness_name(store: &WorkspaceStore, harness: &HarnessId) -> String {
    match harness {
        HarnessId::Native(kind) => provider_label(*kind).to_owned(),
        HarnessId::Acp(id) => store
            .installed_acp_agent(id)
            .map(|agent| agent.name)
            .unwrap_or_else(|| id.clone()),
    }
}

/// Settings → Plugins was asked for, on this profile's tab.
pub(crate) struct ManagePlugins(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Page {
    Overview,
    Profile(String),
    AddAccount,
    AddEndpoint,
}

impl Page {
    fn index(&self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Profile(_) => 1,
            Self::AddAccount => 2,
            Self::AddEndpoint => 3,
        }
    }
}

/// The fields this dialog holds, each committed on blur or Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Binary,
    LaunchArgs,
    ConfigDir,
    Name,
    BaseUrl,
    Model,
    ApiKey,
    OwnBinary,
    OwnArgs,
    CustomModel,
    AddName,
    AddBaseUrl,
    AddModel,
    AddKey,
}

impl Field {
    /// A field of one profile's page, which goes with it.
    fn on_profile_page(self) -> bool {
        matches!(
            self,
            Self::Name
                | Self::BaseUrl
                | Self::Model
                | Self::ApiKey
                | Self::OwnBinary
                | Self::OwnArgs
                | Self::CustomModel
                | Self::ConfigDir
        )
    }

    /// A field that shows what the host holds, rather than a draft.
    fn follows_host(self) -> bool {
        !matches!(
            self,
            Self::ApiKey
                | Self::CustomModel
                | Self::AddName
                | Self::AddBaseUrl
                | Self::AddModel
                | Self::AddKey
        )
    }
}

pub(crate) struct HarnessDialog {
    store: Entity<WorkspaceStore>,
    harness: HarnessId,
    page: Page,
    /// Drawn as a page of the settings stack rather than in a dialog.
    embedded: bool,
    /// Rows stack their labels over their fields: on a page of the compact
    /// stack, or in a dialog on a window too narrow for both side by side.
    narrow: bool,
    fields: HashMap<Field, (Entity<InputState>, Subscription)>,
    /// What each field last sent, so leaving it again before the host's
    /// answer arrives sends nothing more.
    sent: HashMap<Field, String>,
    harness_env: Option<Entity<EnvEditor>>,
    profile_env: Option<Entity<EnvEditor>>,
    models_open: bool,
    slug_error: Option<String>,
    email_revealed: HashSet<String>,
    preset: Option<usize>,
    inherit_config: bool,
    adding: bool,
    add_error: Option<String>,
    copied: crate::widgets::copy::CopiedMark,
    motion: crate::motion::owned::Owned,
    /// What the harness's own configuration lists, once the host has read it.
    native_mcp: Option<Result<Vec<agent::NativeMcpServer>, String>>,
    native_mcp_read: Option<gpui::Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ManagePlugins> for HarnessDialog {}
impl EventEmitter<gpui::DismissEvent> for HarnessDialog {}

impl HarnessDialog {
    pub(crate) fn new(
        store: Entity<WorkspaceStore>,
        harness: HarnessId,
        embedded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.subscribe_in(
            &store,
            window,
            |this, _, change: &StoreChange, window, cx| {
                if matches!(change.topic, TopicKind::Settings | TopicKind::Providers) {
                    this.follow_host(window, cx);
                }
            },
        )];
        let mut dialog = Self {
            store,
            harness,
            page: Page::Overview,
            embedded,
            narrow: embedded,
            fields: HashMap::new(),
            sent: HashMap::new(),
            harness_env: None,
            profile_env: None,
            models_open: false,
            slug_error: None,
            email_revealed: HashSet::new(),
            preset: Some(0),
            inherit_config: true,
            adding: false,
            add_error: None,
            copied: Default::default(),
            motion: Default::default(),
            native_mcp: None,
            native_mcp_read: None,
            _subscriptions: subscriptions,
        };
        dialog.read_native_mcp(cx);
        dialog
    }

    /// Asks the host what the harness's own configuration lists, each time
    /// the dialog opens: the CLI changes that file on its own.
    fn read_native_mcp(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if describe(store, &self.harness).is_none_or(|description| !description.native_mcp_config)
            || !store.scope().is_full()
        {
            return;
        }
        let profile = self.builtin_id().to_owned();
        let read = self
            .store
            .update(cx, |store, cx| store.native_mcp_servers(profile, cx));
        self.native_mcp_read = Some(cx.spawn(async move |this, cx| {
            let servers = read.await;
            let _ = this.update(cx, |this, cx| {
                this.native_mcp = Some(servers);
                cx.notify();
            });
        }));
    }

    pub(crate) fn harness(&self) -> &HarnessId {
        &self.harness
    }

    fn kind(&self) -> ProviderKind {
        self.harness.kind()
    }

    fn builtin_id(&self) -> &'static str {
        Settings::builtin_profile_id(self.kind())
    }

    /// The profile whose account fields the current page shows: the page's
    /// own, or on a harness with one account, that account.
    fn shown_profile(&self) -> String {
        match &self.page {
            Page::Profile(id) => id.clone(),
            _ => self.builtin_id().to_owned(),
        }
    }

    /// Save what was typed, before what holds it goes. The press that leaves
    /// the page or closes the dialog takes the focus first, and the field's
    /// blur would arrive after the field is gone, so every field that differs
    /// from what the host holds is saved here.
    pub(crate) fn flush(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed: Vec<(Field, String)> = self
            .fields
            .iter()
            .filter(|(field, _)| field.follows_host() || **field == Field::ApiKey)
            .map(|(field, (input, _))| (*field, input.read(cx).value().to_string()))
            .collect();
        for (field, value) in typed {
            self.commit(field, value, window, cx);
        }
        for (editor, env) in [
            (self.harness_env.clone(), self.harness_env_value(cx)),
            (self.profile_env.clone(), self.profile_env_value(cx)),
        ] {
            if let Some(editor) = editor
                && !editor.read(cx).shows(&env, cx)
            {
                editor.update(cx, |editor, cx| editor.commit(cx));
            }
        }
    }

    fn navigate(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.flush(window, cx);
        self.fields.retain(|field, _| !field.on_profile_page());
        self.sent.retain(|field, _| !field.on_profile_page());
        self.profile_env = None;
        self.models_open = false;
        self.slug_error = None;
        self.add_error = None;
        self.adding = false;
        if matches!(page, Page::AddAccount | Page::AddEndpoint) {
            self.fields.retain(|field, _| field.follows_host());
            self.inherit_config = true;
        }
        if page == Page::AddEndpoint {
            self.preset = (!self.presets(cx).is_empty()).then_some(0);
            self.inherit_config = false;
            self.seed_preset(window, cx);
        }
        self.page = page;
        window.focus(&cx.focus_handle(), cx);
        cx.notify();
    }

    fn presets(&self, cx: &App) -> Vec<tcode_protocol::EndpointPresetDescription> {
        describe(self.store.read(cx), &self.harness)
            .and_then(|description| description.endpoint)
            .map(|endpoint| endpoint.presets)
            .unwrap_or_default()
    }

    /// Show what the host now holds in every field not being edited.
    fn follow_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Page::Profile(id) = &self.page
            && self.store.read(cx).provider_profile_settings(id) == Default::default()
            && !harness_profiles(self.store.read(cx), &self.harness)
                .iter()
                .any(|profile| &profile.id == id)
        {
            // The profile was deleted, here or elsewhere.
            self.navigate(Page::Overview, window, cx);
        }
        // What the host now holds is no longer waiting for its answer.
        let answered: Vec<Field> = self
            .sent
            .iter()
            .filter(|(field, value)| self.host_value(**field, cx).trim() == value.trim())
            .map(|(field, _)| *field)
            .collect();
        for field in answered {
            self.sent.remove(&field);
        }
        let fields: Vec<(Field, Entity<InputState>)> = self
            .fields
            .iter()
            .filter(|(field, _)| field.follows_host())
            .map(|(field, (input, _))| (*field, input.clone()))
            .collect();
        for (field, input) in fields {
            if input.read(cx).focus_handle(cx).is_focused(window) {
                continue;
            }
            let value = self.host_value(field, cx);
            if input.read(cx).value() != value {
                input.update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
        for (editor, env, secret_id) in [
            (
                self.harness_env.clone(),
                self.harness_env_value(cx),
                self.harness_env_secret_id(cx),
            ),
            (
                self.profile_env.clone(),
                self.profile_env_value(cx),
                self.shown_profile(),
            ),
        ] {
            let Some(editor) = editor else { continue };
            if !editor.read(cx).shows(&env, cx) && !editor.read(cx).focused(window, cx) {
                let stored = self.stored_secrets(&secret_id, cx);
                editor.update(cx, |editor, cx| editor.seed(&env, stored, window, cx));
            }
        }
        cx.notify();
    }

    fn host_value(&self, field: Field, cx: &App) -> String {
        let store = self.store.read(cx);
        let profile = self.shown_profile();
        let path = |path: Option<std::path::PathBuf>| {
            path.map(|path| path.display().to_string())
                .unwrap_or_default()
        };
        match field {
            Field::Binary => match &self.harness {
                HarnessId::Native(_) => path(
                    store
                        .provider_profile_settings(self.builtin_id())
                        .binary_path,
                ),
                HarnessId::Acp(_) => String::new(),
            },
            Field::LaunchArgs => match &self.harness {
                HarnessId::Native(_) => store
                    .provider_profile_settings(self.builtin_id())
                    .launch_args
                    .unwrap_or_default(),
                HarnessId::Acp(id) => store
                    .installed_acp_agent(id)
                    .and_then(|agent| agent.launch_args)
                    .unwrap_or_default(),
            },
            Field::ConfigDir => path(store.provider_profile_settings(&profile).home_path),
            Field::Name => store
                .provider_profile_settings(&profile)
                .display_name
                .unwrap_or_default(),
            Field::BaseUrl | Field::Model => {
                let endpoint =
                    describe(store, &self.harness).and_then(|description| description.endpoint);
                let name = endpoint.and_then(|endpoint| match field {
                    Field::BaseUrl => Some(endpoint.base_url_env),
                    _ => endpoint.model_env,
                });
                name.and_then(|name| {
                    env_value(&store.provider_profile_settings(&profile).env, &name)
                })
                .unwrap_or_default()
            }
            Field::OwnBinary => path(
                store
                    .provider_profile_own_settings(&profile)
                    .and_then(|own| own.binary_path),
            ),
            Field::OwnArgs => store
                .provider_profile_own_settings(&profile)
                .and_then(|own| own.launch_args)
                .unwrap_or_default(),
            Field::ApiKey
            | Field::CustomModel
            | Field::AddName
            | Field::AddBaseUrl
            | Field::AddModel
            | Field::AddKey => String::new(),
        }
    }

    /// The input for `field`, made the first time it is drawn.
    fn input(
        &mut self,
        field: Field,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some((input, _)) = self.fields.get(&field) {
            return input.clone();
        }
        let value = self.host_value(field, cx);
        let masked = matches!(field, Field::ApiKey | Field::AddKey);
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).masked(masked);
            input.set_value(value, window, cx);
            input
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |this, input, event, window, cx| match event {
                InputEvent::PressEnter { .. } if field == Field::CustomModel => {
                    this.add_custom_model(window, cx)
                }
                InputEvent::PressEnter { .. }
                    if matches!(field, Field::AddName | Field::AddKey) =>
                {
                    this.add(window, cx)
                }
                InputEvent::Blur | InputEvent::PressEnter { .. } => {
                    let value = input.read(cx).value().to_string();
                    this.commit(field, value, window, cx);
                }
                _ => {}
            },
        );
        self.fields.insert(field, (input.clone(), subscription));
        input
    }

    fn update_profile(&self, profile_id: String, patch: ProfileSettingsPatch, cx: &mut App) {
        self.store.update(cx, |store, _| {
            store.update_profile_settings(profile_id, patch);
            store.reload_provider();
        });
    }

    fn commit(&mut self, field: Field, value: String, window: &mut Window, cx: &mut Context<Self>) {
        if !field.follows_host() && field != Field::ApiKey {
            return;
        }
        if field != Field::ApiKey
            && (value.trim() == self.host_value(field, cx).trim()
                || self
                    .sent
                    .get(&field)
                    .is_some_and(|sent| sent.trim() == value.trim()))
        {
            return;
        }
        self.sent.insert(field, value.clone());
        let text = Some(value.trim().to_owned()).filter(|value| !value.is_empty());
        let profile = self.shown_profile();
        let builtin = self.builtin_id().to_owned();
        match (field, &self.harness) {
            (Field::Binary, HarnessId::Native(_)) => self.update_profile(
                builtin,
                ProfileSettingsPatch::SetBinaryPath(text.map(Into::into)),
                cx,
            ),
            (Field::LaunchArgs, HarnessId::Native(_)) => {
                self.update_profile(builtin, ProfileSettingsPatch::SetLaunchArgs(text), cx)
            }
            (Field::LaunchArgs, HarnessId::Acp(id)) => {
                let id = id.clone();
                self.store.update(cx, |store, _| {
                    store.update_acp_agent(
                        id,
                        tcode_core::acp::AcpAgentPatch::SetLaunchArgs { launch_args: text },
                    )
                });
            }
            (Field::ConfigDir, _) => self.update_profile(
                profile,
                ProfileSettingsPatch::SetHomePath(text.map(Into::into)),
                cx,
            ),
            (Field::Name, _) => {
                self.update_profile(profile, ProfileSettingsPatch::SetDisplayName(text), cx)
            }
            (Field::OwnBinary, _) => self.update_profile(
                profile,
                ProfileSettingsPatch::SetBinaryPath(text.map(Into::into)),
                cx,
            ),
            (Field::OwnArgs, _) => {
                self.update_profile(profile, ProfileSettingsPatch::SetLaunchArgs(text), cx)
            }
            (Field::BaseUrl | Field::Model, _) => {
                let Some(endpoint) = describe(self.store.read(cx), &self.harness)
                    .and_then(|description| description.endpoint)
                else {
                    return;
                };
                let name = match field {
                    Field::BaseUrl => Some(endpoint.base_url_env),
                    _ => endpoint.model_env,
                };
                let Some(name) = name else { return };
                let settings = self.store.read(cx).provider_profile_settings(&profile);
                let previous = env_value(&settings.env, &name);
                let mut env = settings.env.clone();
                set_env_value(&mut env, &name, text.clone());
                self.update_profile(profile.clone(), ProfileSettingsPatch::SetEnv(env), cx);
                if field == Field::Model {
                    // The endpoint's model is offered in the picker as a custom one.
                    let mut models = settings.custom_models;
                    models.retain(|model| Some(model) != previous.as_ref());
                    if let Some(model) = text.filter(|model| !models.contains(model)) {
                        models.push(model);
                    }
                    self.update_profile(profile, ProfileSettingsPatch::SetCustomModels(models), cx);
                }
            }
            (Field::ApiKey, _) => {
                let (Some(key), Some(endpoint)) = (
                    text,
                    describe(self.store.read(cx), &self.harness)
                        .and_then(|description| description.endpoint),
                ) else {
                    return;
                };
                let settings = self.store.read(cx).provider_profile_settings(&profile);
                if !settings
                    .env
                    .iter()
                    .any(|var| var.name == endpoint.key_env && var.sensitive)
                {
                    let mut env = settings.env;
                    env.retain(|var| var.name != endpoint.key_env);
                    env.push(EnvVar {
                        name: endpoint.key_env.clone(),
                        value: String::new(),
                        sensitive: true,
                    });
                    self.store.update(cx, |store, _| {
                        store.update_profile_settings(
                            profile.clone(),
                            ProfileSettingsPatch::SetEnv(env),
                        )
                    });
                }
                self.store.update(cx, |store, _| {
                    store.set_profile_secret(profile, endpoint.key_env, Some(key));
                    store.reload_provider();
                });
                if let Some((input, _)) = self.fields.get(&Field::ApiKey) {
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                }
            }
            _ => {}
        }
    }

    fn stored_secrets(&self, id: &str, cx: &App) -> HashSet<String> {
        self.store.read(cx).provider_profile_stored_secret_names(id)
    }

    /// What the variables say while the host's saved secrets cannot be read,
    /// where no secret shows as saved.
    fn secrets_unreadable(&self, cx: &App) -> Option<AnyElement> {
        let error = self
            .store
            .read(cx)
            .provider_secrets_unreadable()?
            .to_owned();
        Some(
            div()
                .debug_selector(|| "harness-secrets-unreadable".into())
                .text_secondary(false, cx)
                .child(crate::tr!(
                    "providers.env.secrets_unreadable",
                    error = error
                ))
                .into_any_element(),
        )
    }

    /// The variables the overview edits: on a harness with one account, that
    /// account's own; else the ones every account shares.
    fn harness_env_value(&self, cx: &App) -> Vec<EnvVar> {
        let store = self.store.read(cx);
        match &self.harness {
            HarnessId::Acp(id) => store
                .installed_acp_agent(id)
                .map(|agent| agent.env)
                .unwrap_or_default(),
            HarnessId::Native(_) => {
                let builtin = store.provider_profile_settings(self.builtin_id());
                if self.lists_accounts(cx) {
                    builtin.shared_env
                } else {
                    builtin.env
                }
            }
        }
    }

    fn harness_env_secret_id(&self, cx: &App) -> String {
        match &self.harness {
            HarnessId::Acp(id) => Settings::acp_secret_id(id),
            HarnessId::Native(kind) if self.lists_accounts(cx) => {
                Settings::harness_secret_id(*kind)
            }
            HarnessId::Native(_) => self.builtin_id().to_owned(),
        }
    }

    /// The page's own variables, without the endpoint's, which have fields
    /// of their own.
    fn profile_env_value(&self, cx: &App) -> Vec<EnvVar> {
        let store = self.store.read(cx);
        let endpoint_names = describe(store, &self.harness)
            .map(|description| endpoint_names(&description))
            .unwrap_or_default();
        store
            .provider_profile_settings(&self.shown_profile())
            .env
            .into_iter()
            .filter(|var| !self.is_endpoint_profile(cx) || !endpoint_names.contains(&var.name))
            .collect()
    }

    fn lists_accounts(&self, cx: &App) -> bool {
        let store = self.store.read(cx);
        describe(store, &self.harness).is_some_and(|description| {
            lists_accounts(&description, harness_profiles(store, &self.harness).len())
        })
    }

    fn is_endpoint_profile(&self, cx: &App) -> bool {
        let id = self.shown_profile();
        harness_profiles(self.store.read(cx), &self.harness)
            .iter()
            .any(|profile| profile.id == id && profile.is_endpoint_profile())
    }

    fn harness_env(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<EnvEditor> {
        if let Some(editor) = &self.harness_env {
            return editor.clone();
        }
        let env = self.harness_env_value(cx);
        let secret_id = self.harness_env_secret_id(cx);
        let stored = self.stored_secrets(&secret_id, cx);
        let store = self.store.clone();
        let harness = self.harness.clone();
        let builtin = self.builtin_id().to_owned();
        let shared = self.lists_accounts(cx);
        let commit: CommitEnv = std::rc::Rc::new(move |env, secrets, cx| {
            store.update(cx, |store, _| {
                match &harness {
                    HarnessId::Acp(id) => store.update_acp_agent(
                        id.clone(),
                        tcode_core::acp::AcpAgentPatch::SetEnv { env },
                    ),
                    HarnessId::Native(_) => store.update_profile_settings(
                        builtin.clone(),
                        if shared {
                            ProfileSettingsPatch::SetSharedEnv(env)
                        } else {
                            ProfileSettingsPatch::SetEnv(env)
                        },
                    ),
                }
                for (name, value) in secrets {
                    store.set_profile_secret(secret_id.clone(), name, value);
                }
                store.reload_provider();
            });
        });
        let key = format!("{}-env", self.harness.key());
        let editor = cx.new(|cx| EnvEditor::new(key, &env, stored, commit, window, cx));
        self.harness_env = Some(editor.clone());
        editor
    }

    fn profile_env(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<EnvEditor> {
        if let Some(editor) = &self.profile_env {
            return editor.clone();
        }
        let profile = self.shown_profile();
        let env = self.profile_env_value(cx);
        let stored = self.stored_secrets(&profile, cx);
        let store = self.store.clone();
        let endpoint_names = describe(self.store.read(cx), &self.harness)
            .map(|description| endpoint_names(&description))
            .unwrap_or_default();
        let keeps_endpoint = self.is_endpoint_profile(cx);
        let id = profile.clone();
        let commit: CommitEnv = std::rc::Rc::new(move |env, secrets, cx| {
            store.update(cx, |store, _| {
                // The endpoint's own variables stay as their fields left them.
                let mut saved: Vec<EnvVar> = if keeps_endpoint {
                    store
                        .provider_profile_settings(&id)
                        .env
                        .into_iter()
                        .filter(|var| endpoint_names.contains(&var.name))
                        .collect()
                } else {
                    Vec::new()
                };
                saved.extend(env);
                store.update_profile_settings(id.clone(), ProfileSettingsPatch::SetEnv(saved));
                for (name, value) in secrets {
                    store.set_profile_secret(id.clone(), name, value);
                }
                store.reload_provider();
            });
        });
        let key = format!("{}-{profile}-env", self.harness.key());
        let editor = cx.new(|cx| EnvEditor::new(key, &env, stored, commit, window, cx));
        self.profile_env = Some(editor.clone());
        editor
    }

    fn recheck(&self, cx: &mut App) {
        self.store.update(cx, |store, _| {
            store.refresh_provider_status();
            store.check_provider_versions();
            store.refresh_acp_registry();
        });
    }

    fn add_custom_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, _)) = self.fields.get(&Field::CustomModel) else {
            return;
        };
        let input = input.clone();
        let raw = input.read(cx).value().to_string();
        let profile = self.shown_profile();
        let store = self.store.read(cx);
        let catalog = store.profile_catalog(&profile);
        let settings = store.provider_profile_settings(&profile);
        match validate_slug(&raw, &catalog, &settings) {
            Ok(slug) => {
                self.slug_error = None;
                let mut models = settings.custom_models;
                models.push(slug);
                self.update_profile(profile, ProfileSettingsPatch::SetCustomModels(models), cx);
                input.update(cx, |input, cx| input.set_value("", window, cx));
            }
            Err(error) => self.slug_error = Some(slug_error_message(&error)),
        }
        cx.notify();
    }

    fn seed_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let preset = self
            .preset
            .and_then(|index| self.presets(cx).get(index).cloned());
        let values = [
            (Field::AddName, preset.as_ref().map(|p| p.name.clone())),
            (
                Field::AddBaseUrl,
                preset.as_ref().map(|p| p.base_url.clone()),
            ),
            (
                Field::AddModel,
                preset.as_ref().and_then(|p| p.model.clone()),
            ),
            (Field::AddKey, None),
        ];
        for (field, value) in values {
            let input = self.input(field, window, cx);
            input.update(cx, |input, cx| {
                input.set_value(value.unwrap_or_default(), window, cx)
            });
        }
    }

    /// Add the account or endpoint profile the page's form describes.
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = |this: &Self, field: Field, cx: &App| {
            this.fields
                .get(&field)
                .map(|(input, _)| input.read(cx).value().trim().to_string())
                .unwrap_or_default()
        };
        let kind = self.kind();
        let inherit = self.inherit_config;
        match self.page {
            Page::AddEndpoint => {
                let (name, base_url, model, key) = (
                    value(self, Field::AddName, cx),
                    value(self, Field::AddBaseUrl, cx),
                    value(self, Field::AddModel, cx),
                    value(self, Field::AddKey, cx),
                );
                if base_url.is_empty() || key.is_empty() {
                    self.add_error =
                        Some(crate::tr!("providers.harness.endpoint_required").into_owned());
                    cx.notify();
                    return;
                }
                self.store.update(cx, |store, _| {
                    store.create_third_party_profile(
                        kind,
                        name,
                        base_url,
                        Some(model).filter(|model| !model.is_empty()),
                        key,
                        inherit,
                    )
                });
                self.navigate(Page::Overview, window, cx);
            }
            Page::AddAccount => {
                let name = value(self, Field::AddName, cx);
                if self.adding || name.is_empty() {
                    return;
                }
                let create = self.store.update(cx, |store, cx| {
                    store.create_account_profile(kind, name, inherit, cx)
                });
                self.adding = true;
                self.add_error = None;
                cx.notify();
                cx.spawn_in(window, async move |this, cx| {
                    let response = create.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.adding = false;
                        match response {
                            Ok(tcode_protocol::CommandResponse::ProfileId(profile_id)) => {
                                this.navigate(Page::Profile(profile_id.clone()), window, cx);
                                crate::provider_login_dialog::show(
                                    this.store.clone(),
                                    kind,
                                    profile_id,
                                    window,
                                    cx,
                                );
                            }
                            Ok(other) => {
                                log::error!("unexpected create-account response: {other:?}");
                                this.add_error = Some(
                                    crate::tr!("providers.account.create_failed").into_owned(),
                                );
                            }
                            Err(error) => {
                                this.add_error = Some(match error.code.as_str() {
                                    "profile_exists" => {
                                        crate::tr!("providers.account.profile_exists").into_owned()
                                    }
                                    _ => crate::tr!(
                                        "providers.account.create_failed_reason",
                                        reason = error.message
                                    )
                                    .into_owned(),
                                });
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            _ => {}
        }
    }

    fn manage_plugins(&self, profile_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.embedded {
            window.close_dialog(cx);
        }
        cx.emit(ManagePlugins(profile_id));
    }

    fn confirm_delete(&self, profile_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let endpoint = self.is_endpoint_profile(cx);
        let (title, body) = delete_confirmation(store.read(cx), &profile_id, self.kind(), endpoint);
        let dialog = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let profile_id = profile_id.clone();
            let dialog = dialog.clone();
            alert
                .title(title.clone())
                .description(body.clone())
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(if endpoint {
                            crate::tr!("providers.harness.delete_endpoint")
                        } else {
                            crate::tr!("providers.harness.delete_account")
                        })
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let profile_id = profile_id.clone();
                    store.update(cx, |store, _| store.delete_profile(profile_id));
                    window.close_dialog(cx);
                    _ = dialog.update(cx, |dialog, cx| {
                        dialog.navigate(Page::Overview, window, cx);
                    });
                    true
                })
        });
    }

    fn confirm_remove_agent(&self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let name = harness_name(store.read(cx), &self.harness);
        let dialog = cx.entity().downgrade();
        let embedded = self.embedded;
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let id = id.clone();
            let dialog = dialog.clone();
            alert
                .title(crate::tr!("providers.harness.remove_title", name = name).into_owned())
                .description(crate::tr!("providers.harness.remove_body"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("providers.acp.remove"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let id = id.clone();
                    store.update(cx, |store, _| store.remove_acp_agent(id));
                    if embedded {
                        window.close_dialog(cx);
                        _ = dialog.update(cx, |_, cx| cx.emit(gpui::DismissEvent));
                    } else {
                        window.close_all_dialogs(cx);
                    }
                    true
                })
        });
    }

    fn confirm_sign_out(&self, profile_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let name = store.read(cx).provider_profile_display_name(&profile_id);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let profile_id = profile_id.clone();
            alert
                .title(
                    crate::tr!("providers.login.sign_out_confirm_title", name = name).into_owned(),
                )
                .description(crate::tr!("providers.login.sign_out_confirm_body"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("providers.login.sign_out"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let profile_id = profile_id.clone();
                    store.update(cx, |store, _| store.logout_provider(profile_id));
                    true
                })
        });
    }
}

/// The dialog of `harness`, on a wide window.
pub(crate) fn open(
    store: Entity<WorkspaceStore>,
    harness: HarnessId,
    window: &mut Window,
    cx: &mut App,
) -> Entity<HarnessDialog> {
    let dialog = cx.new(|cx| HarnessDialog::new(store, harness, false, window, cx));
    let shown = dialog.clone();
    window.open_dialog(cx, move |builder, _, cx| {
        let content = shown.clone();
        let closing = shown.clone();
        let title = shown.update(cx, |dialog, cx| dialog.render_title(cx));
        builder
            .title(title)
            .wide()
            .content(move |body, _, _| body.child(content.clone()))
            .on_close(move |_, window, cx| {
                closing.update(cx, |dialog, cx| dialog.flush(window, cx));
            })
    });
    dialog
}

// Rendering.
impl HarnessDialog {
    fn compact(&self) -> bool {
        self.narrow
    }

    /// The dialog's title: on the overview, the harness with its version and
    /// the binary it runs; on another page, the way back and that page.
    pub(crate) fn render_title(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let description = describe(store, &self.harness);
        let name = harness_name(store, &self.harness);
        let back = |label: String, cx: &mut Context<Self>| {
            Button::new("harness-back")
                .debug_selector(|| "harness-back".into())
                .ghost()
                .xsmall()
                .icon(IconName::ChevronLeft)
                .label(label)
                .on_click(
                    cx.listener(|this, _, window, cx| this.navigate(Page::Overview, window, cx)),
                )
        };
        let page_title = match &self.page {
            Page::Overview => None,
            Page::Profile(id) => Some(store.provider_profile_short_name(id)),
            Page::AddAccount => Some(crate::tr!("providers.harness.add_account").into_owned()),
            Page::AddEndpoint => Some(crate::tr!("providers.harness.add_endpoint").into_owned()),
        };
        if let Some(title) = page_title {
            let endpoint = matches!(self.page, Page::Profile(_)) && self.is_endpoint_profile(cx);
            return v_flex()
                .min_w_0()
                .gap_1()
                .items_start()
                .child(div().text_secondary(false, cx).child(back(name, cx)))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .when_some(
                            match &self.page {
                                Page::Profile(id) => Some(crate::provider_card::profile_glyph(
                                    self.store.read(cx),
                                    self.kind(),
                                    Some(id),
                                )),
                                _ => None,
                            },
                            |row, glyph| row.child(glyph),
                        )
                        .child(div().text_title(cx).child(crate::widgets::fade_text(title)))
                        .when(endpoint, |row| row.child(endpoint_chip(cx))),
                )
                .into_any_element();
        }
        self.render_header(description.as_ref(), name, cx)
    }

    fn render_header(
        &self,
        description: Option<&HarnessDescription>,
        name: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let snapshot = store.provider_profile_snapshot(self.builtin_id());
        let version = match &self.harness {
            HarnessId::Native(kind) => snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.version.clone())
                .or_else(|| {
                    store
                        .provider_version_status(*kind)
                        .and_then(|v| v.installed)
                }),
            HarnessId::Acp(id) => store
                .installed_acp_agent(id)
                .map(|agent| agent.version)
                .filter(|version| !version.is_empty()),
        };
        let checking = snapshot.as_ref().is_none_or(|snapshot| snapshot.checking)
            && matches!(self.harness, HarnessId::Native(_));
        let second = match description.and_then(|description| description.launch_summary.as_ref()) {
            Some(summary) => Some(summary.clone()),
            None => snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.binary.clone()),
        };
        let glyph = crate::provider_card::provider_glyph(self.kind());
        h_flex()
            .w_full()
            .min_w_0()
            .gap_3()
            .items_center()
            .child(div().flex_none().text_body(false, cx).child(glyph))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_title(cx).child(crate::widgets::fade_text(name)))
                            .children(version.map(|version| {
                                div()
                                    .text_meta_mono(false, cx)
                                    .child(format!("v{}", version.trim_start_matches('v')))
                            }))
                            .when(matches!(self.harness, HarnessId::Acp(_)), |row| {
                                row.child(crate::material::chip("ACP", cx))
                            }),
                    )
                    .children(second.map(|second| {
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_meta_mono(false, cx)
                            .child(crate::widgets::fade_text(second))
                    })),
            )
            .child(
                Button::new("harness-recheck")
                    .ghost()
                    .xsmall()
                    .loading(checking)
                    .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                    .tooltip(crate::tr!("providers.harness.recheck"))
                    .on_click(cx.listener(|this, _, _, cx| this.recheck(cx))),
            )
            .into_any_element()
    }

    fn group(&self, caption: Option<String>, rows: Vec<AnyElement>, cx: &App) -> AnyElement {
        v_flex()
            .w_full()
            .when_some(caption, |group, caption| {
                group.child(crate::material::group_caption(caption, cx))
            })
            .child(crate::material::grouped(rows, cx))
            .into_any_element()
    }

    fn row(
        &self,
        title: impl Into<SharedString>,
        description: Option<String>,
        control: Option<AnyElement>,
        cx: &App,
    ) -> AnyElement {
        let compact = self.compact();
        crate::settings_rows::row_frame(compact, cx)
            .child(row_labels(
                title,
                description.map(SharedString::from),
                None,
                compact,
                cx,
            ))
            .children(control.map(|control| {
                if compact {
                    div().w_full().child(control)
                } else {
                    div().flex_none().w(px(FIELD_WIDTH)).child(control)
                }
            }))
            .into_any_element()
    }

    /// A row whose control is a field committed as it is left.
    fn field_row(
        &mut self,
        field: Field,
        title: impl Into<SharedString>,
        description: Option<String>,
        placeholder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input = self.input(field, window, cx);
        let selector = format!("harness-field-{field:?}");
        let control = div()
            .w_full()
            .debug_selector(move || selector.clone())
            .child(
                Input::new(&input).when_some(placeholder, |input, placeholder| {
                    input.placeholder(placeholder)
                }),
            )
            .into_any_element();
        self.row(title, description, Some(control), cx)
    }

    fn render_banners(
        &self,
        description: &HarnessDescription,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut banners = Vec::new();
        let store = self.store.read(cx);
        let builtin = self.builtin_id();
        let theme = cx.theme().clone();
        let enabled = match &self.harness {
            HarnessId::Native(_) => harness_profiles(store, &self.harness)
                .iter()
                .any(|profile| profile.settings.enabled),
            HarnessId::Acp(id) => store
                .installed_acp_agent(id)
                .is_some_and(|agent| agent.enabled),
        };
        let snapshot = store.provider_profile_snapshot(builtin);
        let failed = snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.installed && snapshot.diagnostic == Some(ProviderProbeDiagnostic::FailedCli)
        });
        if failed {
            let snapshot = snapshot.clone().unwrap_or_default();
            let binary = description.binary_name.clone().unwrap_or_default();
            let command = format!("{} --version", snapshot.binary.clone().unwrap_or(binary));
            let output = snapshot.message.clone().unwrap_or_default();
            let diagnostic = format!("{command}\n{output}").trim().to_owned();
            banners.push(
                crate::material::notice(theme.danger, cx)
                    .debug_selector(|| "harness-banner-failed".into())
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .text_body(false, cx)
                                    .child(crate::tr!("providers.harness.failed")),
                            )
                            .child(div().text_code(cx).child(command))
                            .when(!output.is_empty(), |body| {
                                body.child(div().text_code(cx).child(output))
                            })
                            .when_some(description.doctor_command.clone(), |body, doctor| {
                                body.child(
                                    div().text_secondary(false, cx).child(
                                        crate::tr!("providers.harness.doctor", command = doctor)
                                            .into_owned(),
                                    ),
                                )
                            }),
                    )
                    .child(
                        Button::new("harness-copy-diagnostic")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("providers.harness.copy_diagnostic"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(diagnostic.clone()))
                            }),
                    )
                    .into_any_element(),
            );
        }
        if let Some(detail) = snapshot.as_ref().and_then(|snapshot| {
            crate::provider_status::outdated_detail(&harness_name(store, &self.harness), snapshot)
        }) {
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .debug_selector(|| "harness-banner-outdated".into())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .child(detail),
                    )
                    .into_any_element(),
            );
        }
        let attention = snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.status == Some(ProviderStatusKind::Warning)
                && snapshot.diagnostic == Some(ProviderProbeDiagnostic::IndeterminateAuth)
        });
        if attention && !self.lists_accounts(cx) {
            let snapshot = snapshot.clone().unwrap_or_default();
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .debug_selector(|| "harness-banner-attention".into())
                    .child(div().flex_1().min_w_0().text_secondary(false, cx).child(
                        crate::provider_status::snapshot_detail(description, &snapshot),
                    ))
                    .into_any_element(),
            );
        }
        let shared_credentials = harness_profiles(store, &self.harness)
            .iter()
            .any(|profile| {
                store
                    .provider_profile_snapshot(&profile.id)
                    .is_some_and(|snapshot| {
                        snapshot.diagnostic == Some(ProviderProbeDiagnostic::SharedCredentials)
                    })
            });
        if shared_credentials {
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.status.shared_credentials")),
                    )
                    .into_any_element(),
            );
        }
        if let HarnessId::Native(kind) = &self.harness
            && let Some(version) = store.provider_version_status(*kind)
            && (version.update_available || version.updating)
        {
            let kind = *kind;
            let latest = version.latest.clone().unwrap_or_default();
            let command = version.update_command.clone();
            let automatic = command.is_some() && !version.update_requires_terminal;
            let updating = version.updating;
            let mut banner = crate::material::notice(theme.info, cx)
                .debug_selector(|| "harness-banner-update".into())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(false, cx)
                        .child(if updating {
                            crate::tr!("providers.harness.updating", version = latest).into_owned()
                        } else {
                            crate::tr!("providers.harness.update_available", version = latest)
                                .into_owned()
                        }),
                );
            if automatic {
                banner = banner.child(
                    Button::new("harness-update")
                        .primary()
                        .xsmall()
                        .loading(updating)
                        .label(crate::tr!("providers.update_now"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store
                                .update(cx, |store, _| store.update_providers(vec![kind]))
                        })),
                );
            }
            let mut column = v_flex().w_full().gap_2().child(banner);
            if let Some(command) = command.filter(|_| !automatic) {
                column = column.child(self.command_line("harness-update-command", command, cx));
            }
            banners.push(column.into_any_element());
        }
        if let Some(latest) = description.registry_update.clone()
            && let HarnessId::Acp(id) = &self.harness
        {
            let id = id.clone();
            let installing = self
                .store
                .read(cx)
                .acp_marketplace_items()
                .iter()
                .any(|item| item.id == id && item.installing);
            banners.push(
                crate::material::notice(theme.info, cx)
                    .debug_selector(|| "harness-banner-registry-update".into())
                    .child(
                        div().flex_1().min_w_0().text_body(false, cx).child(
                            crate::tr!("providers.harness.registry_update", version = latest)
                                .into_owned(),
                        ),
                    )
                    .child(
                        Button::new("harness-registry-update")
                            .primary()
                            .xsmall()
                            .loading(installing)
                            .label(crate::tr!("providers.update_now"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = id.clone();
                                this.store
                                    .update(cx, |store, _| store.install_acp_agent(id))
                            })),
                    )
                    .into_any_element(),
            );
        }
        if !enabled {
            banners.push(
                crate::material::notice(theme.warning, cx)
                    .debug_selector(|| "harness-banner-disabled".into())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_body(false, cx)
                            .child(crate::tr!("providers.harness.disabled")),
                    )
                    .child(
                        Button::new("harness-enable")
                            .xsmall()
                            .label(crate::tr!("providers.harness.enable"))
                            .on_click(cx.listener(|this, _, _, cx| this.set_enabled(true, cx))),
                    )
                    .into_any_element(),
            );
        }
        banners
    }

    pub(crate) fn set_enabled(&self, enabled: bool, cx: &mut App) {
        set_harness_enabled(&self.store, &self.harness, enabled, cx);
    }

    /// A command to run in a terminal, with its copy button.
    fn command_line(
        &self,
        id: &'static str,
        command: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied = self.copied.is(id);
        let copy = command.clone();
        h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .rounded(crate::material::radius_input(cx))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted)
            .pl_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_code(cx)
                    .child(crate::widgets::fade_text(command)),
            )
            .child(crate::widgets::copy::copy_button(
                id,
                copied,
                self.compact(),
                cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    this.copied
                        .mark(id.to_owned(), |this: &mut Self| &mut this.copied, cx);
                }),
                cx,
            ))
            .into_any_element()
    }

    /// The whole overview of a harness whose CLI the host cannot find: how
    /// to install it, or where it already is.
    fn render_install(
        &mut self,
        description: &HarnessDescription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let binary = description.binary_name.clone().unwrap_or_default();
        let mut rows = v_flex()
            .w_full()
            .gap_3()
            .debug_selector(|| "harness-install".into());
        rows = rows.child(
            div()
                .text_body(false, cx)
                .child(crate::tr!("providers.harness.not_found", binary = binary).into_owned()),
        );
        if let Some(command) = description.install_command.clone() {
            rows = rows
                .child(
                    div()
                        .text_caption(cx)
                        .child(crate::tr!("providers.harness.install")),
                )
                .child(self.command_line("harness-install-command", command, cx))
                .child(
                    div()
                        .text_secondary(false, cx)
                        .child(crate::tr!("providers.harness.install_help")),
                );
        }
        let input = self.input(Field::Binary, window, cx);
        rows.child(self.group(
            None,
            vec![
                    self.row(
                        crate::tr!("providers.harness.elsewhere").into_owned(),
                        None,
                        Some(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Input::new(&input).placeholder(crate::tr!(
                                            "providers.harness.binary_path"
                                        )),
                                    ),
                                )
                                .child(
                                    Button::new("harness-install-recheck")
                                        .outline()
                                        .small()
                                        .label(crate::tr!("providers.harness.recheck"))
                                        .on_click(cx.listener(|this, _, _, cx| this.recheck(cx))),
                                )
                                .into_any_element(),
                        ),
                        cx,
                    ),
                ],
            cx,
        ))
        .into_any_element()
    }

    fn render_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let Some(description) = describe(store, &self.harness) else {
            return crate::material::empty_row(crate::tr!("composer.loading"), cx)
                .into_any_element();
        };
        let snapshot = store.provider_profile_snapshot(self.builtin_id());
        let missing = description.acp_agent_id.is_none()
            && snapshot.as_ref().is_some_and(|snapshot| {
                !snapshot.checking && snapshot.status.is_some() && !snapshot.installed
            });
        let mut column = v_flex().w_full().gap(crate::material::section_gap(cx));
        let banners = self.render_banners(&description, cx);
        if !banners.is_empty() {
            column = column.child(v_flex().w_full().gap_2().children(banners));
        }
        if missing {
            return column
                .child(self.render_install(&description, window, cx))
                .into_any_element();
        }
        if description.acp_agent_id.is_none() {
            if self.lists_accounts(cx) {
                column = column.child(self.render_accounts(&description, window, cx));
            } else {
                let profile = self.builtin_id().to_owned();
                column = column
                    .children(self.render_sign_in(&description, &profile, window, cx))
                    .child(self.render_models(&profile, window, cx));
            }
        }
        column = column.child(self.render_run(&description, window, cx));
        column = column.children(self.render_native_mcp(cx));
        if let Some(integrations) = self.render_integrations(&description, cx) {
            column = column.child(integrations);
        }
        if let HarnessId::Acp(id) = self.harness.clone() {
            let name = harness_name(self.store.read(cx), &self.harness);
            column = column.child(self.danger_row(
                "harness-remove-agent",
                crate::tr!("providers.harness.remove_title", name = name).into_owned(),
                crate::tr!("providers.harness.remove_body").into_owned(),
                crate::tr!("providers.harness.remove").into_owned(),
                cx.listener(move |this, _, window, cx| {
                    this.confirm_remove_agent(id.clone(), window, cx)
                }),
                cx,
            ));
        }
        column.into_any_element()
    }

    fn render_accounts(
        &mut self,
        description: &HarnessDescription,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let profiles = harness_profiles(store, &self.harness);
        let builtin = self.builtin_id();
        let mut rows = Vec::new();
        for profile in &profiles {
            rows.push(self.account_row(description, profile, profile.id == builtin, cx));
        }
        let account_login = description.account_login;
        let endpoints = description.offers_endpoint_profiles();
        let build_actions = |cx: &mut Context<Self>| {
            let mut actions = h_flex().gap_1();
            if account_login {
                actions = actions.child(
                    Button::new("harness-add-account")
                        .debug_selector(|| "harness-add-account".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label(crate::tr!("providers.harness.add_account"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.navigate(Page::AddAccount, window, cx)
                        })),
                );
            }
            if endpoints {
                actions = actions.child(
                    Button::new("harness-add-endpoint")
                        .debug_selector(|| "harness-add-endpoint".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label(crate::tr!("providers.harness.add_endpoint"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.navigate(Page::AddEndpoint, window, cx)
                        })),
                );
            }
            actions
        };
        let compact = self.compact();
        let actions = build_actions(cx);
        v_flex()
            .w_full()
            .debug_selector(|| "harness-accounts".into())
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .child(crate::material::group_caption(
                        if endpoints {
                            crate::tr!("providers.harness.accounts")
                        } else {
                            crate::tr!("providers.harness.accounts_only")
                        }
                        .into_owned(),
                        cx,
                    ))
                    .when(!compact, |row| row.child(actions)),
            )
            .child(crate::material::grouped(rows, cx))
            .when(compact, |column| column.child(build_actions(cx).pt_2()))
            .into_any_element()
    }

    fn account_row(
        &self,
        description: &HarnessDescription,
        profile: &ResolvedProfile,
        is_default: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let snapshot = store.provider_profile_snapshot(&profile.id);
        let endpoint = profile.is_endpoint_profile();
        let name = store.provider_profile_short_name(&profile.id);
        let detail = if endpoint {
            endpoint_summary(description, &profile.settings.env)
        } else {
            account_summary(snapshot.as_ref())
        };
        let signed_out = !endpoint
            && description.account_login
            && snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.installed
                    && snapshot
                        .auth
                        .as_ref()
                        .is_some_and(|auth| auth.status == AuthStatus::Unauthenticated)
            });
        let id = profile.id.clone();
        let open_id = id.clone();
        let login_id = id.clone();
        let kind = profile.kind;
        let label = name.clone();
        crate::material::accessible_clickable(
            crate::material::group_row(cx),
            SharedString::from(format!("harness-account-{id}")),
            gpui::Role::Button,
            label,
            cx,
        )
        .debug_selector({
            let id = id.clone();
            move || format!("harness-account-{id}")
        })
        .cursor_pointer()
        .relative()
        .child(crate::material::row_hover(px(0.), cx))
        .child(crate::provider_card::profile_glyph(store, kind, Some(&id)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_body(false, cx)
                                .child(crate::widgets::fade_text(name)),
                        )
                        .when(is_default, |row| {
                            row.child(crate::material::chip(
                                crate::tr!("providers.harness.default").into_owned(),
                                cx,
                            ))
                        })
                        .when(endpoint, |row| row.child(endpoint_chip(cx))),
                )
                .when(!detail.is_empty(), |column| {
                    column.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_secondary(false, cx)
                            .child(crate::widgets::fade_text(detail)),
                    )
                }),
        )
        .when(signed_out, |row| {
            row.child(
                Button::new(SharedString::from(format!("harness-sign-in-{login_id}")))
                    .primary()
                    .xsmall()
                    .label(crate::tr!("providers.login.sign_in"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        crate::provider_login_dialog::open(
                            this.store.clone(),
                            kind,
                            login_id.clone(),
                            window,
                            cx,
                        );
                    })),
            )
        })
        .child(Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.navigate(Page::Profile(open_id.clone()), window, cx)
        }))
        .into_any_element()
    }

    /// Who an account is signed in as, and how to sign it in or out.
    fn render_sign_in(
        &mut self,
        description: &HarnessDescription,
        profile_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let snapshot = store.provider_profile_snapshot(profile_id);
        // Sign-in the CLI could not report is said by the banner over it.
        let unknown = snapshot.as_ref().is_some_and(|snapshot| {
            !snapshot.checking
                && snapshot.installed
                && snapshot
                    .auth
                    .as_ref()
                    .is_none_or(|auth| auth.status == AuthStatus::Unknown)
        });
        if unknown {
            return None;
        }
        let manages =
            description.account_login && store.provider_profile_supports_account_login(profile_id);
        let account = snapshot.as_ref().and_then(signed_in_account).cloned();
        let signed_in = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.auth.as_ref())
            .is_some_and(|auth| auth.status == AuthStatus::Authenticated);
        let kind = self.kind();
        let id = profile_id.to_owned();
        let row = if signed_in {
            let revealed = self.email_revealed.contains(profile_id);
            let label = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.auth.as_ref())
                .and_then(|auth| auth.label.clone())
                .filter(|label| !label.is_empty());
            let email = account.and_then(|auth| auth.email);
            let reveal_id = id.clone();
            let title: AnyElement = match email {
                Some(email) => Button::new(SharedString::from(format!("harness-email-{id}")))
                    .text()
                    .label(if revealed {
                        email
                    } else {
                        redact_email(&email)
                    })
                    .tooltip(if revealed {
                        crate::tr!("providers.hide_email")
                    } else {
                        crate::tr!("providers.reveal_email")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.email_revealed.remove(&reveal_id) {
                            this.email_revealed.insert(reveal_id.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
                None => div()
                    .text_body(false, cx)
                    .child(crate::widgets::fade_text(crate::tr!(
                        "providers.status.authenticated"
                    )))
                    .into_any_element(),
            };
            let hint = (!manages)
                .then(|| description.sign_in_command.clone())
                .flatten()
                .map(|command| {
                    crate::tr!("providers.harness.switch_account", command = command).into_owned()
                });
            let sign_out_id = id.clone();
            crate::material::group_row(cx)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(title)
                                .when_some(label, |row, label| {
                                    row.child(crate::material::chip(label, cx))
                                }),
                        )
                        .when_some(hint, |column, hint| {
                            column.child(div().text_secondary(false, cx).child(hint))
                        }),
                )
                .when(manages, |row| {
                    row.child(
                        Button::new("harness-sign-out")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("providers.login.sign_out"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_sign_out(sign_out_id.clone(), window, cx)
                            })),
                    )
                })
                .into_any_element()
        } else {
            let detail = match &snapshot {
                Some(snapshot) if !snapshot.checking && snapshot.status.is_some() => {
                    crate::provider_status::snapshot_detail(description, snapshot)
                }
                _ => crate::tr!("providers.status.checking").into_owned(),
            };
            let detail = if manages {
                crate::tr!("providers.harness.sign_in_help").into_owned()
            } else {
                detail
            };
            crate::material::group_row(cx)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_body(false, cx)
                                .child(crate::tr!("providers.harness.signed_out")),
                        )
                        .child(div().text_secondary(false, cx).child(detail)),
                )
                .when(manages, |row| {
                    row.child(
                        Button::new("harness-sign-in")
                            .debug_selector(|| "harness-sign-in".into())
                            .primary()
                            .xsmall()
                            .label(crate::tr!("providers.login.sign_in"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                crate::provider_login_dialog::open(
                                    this.store.clone(),
                                    kind,
                                    id.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
                })
                .into_any_element()
        };
        Some(self.group(
            Some(crate::tr!("providers.harness.account").into_owned()),
            vec![row],
            cx,
        ))
    }

    fn render_endpoint(
        &mut self,
        description: &HarnessDescription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(endpoint) = description.endpoint.clone() else {
            return div().into_any_element();
        };
        let profile = self.shown_profile();
        let stored = self
            .stored_secrets(&profile, cx)
            .contains(&endpoint.key_env);
        let mut rows = vec![self.field_row(
            Field::BaseUrl,
            crate::tr!("providers.third_party.base_url").into_owned(),
            Some(endpoint.base_url_env.clone()),
            None,
            window,
            cx,
        )];
        rows.push(self.field_row(
            Field::ApiKey,
            crate::tr!("providers.third_party.key").into_owned(),
            Some(endpoint.key_env.clone()),
            Some(if stored {
                crate::tr!("providers.harness.key_saved").into_owned()
            } else {
                crate::tr!("providers.third_party.key_hint").into_owned()
            }),
            window,
            cx,
        ));
        if let Some(model_env) = endpoint.model_env.clone() {
            rows.push(self.field_row(
                Field::Model,
                crate::tr!("providers.third_party.model").into_owned(),
                Some(model_env),
                None,
                window,
                cx,
            ));
        }
        self.group(
            Some(crate::tr!("providers.harness.endpoint").into_owned()),
            rows,
            cx,
        )
    }

    fn render_identity(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown_as = self
            .store
            .read(cx)
            .provider_profile_display_name(profile_id);
        let name = self.field_row(
            Field::Name,
            crate::tr!("providers.harness.name").into_owned(),
            Some(crate::tr!("providers.harness.shown_as", name = shown_as).into_owned()),
            None,
            window,
            cx,
        );
        let accent = self.render_accent(profile_id, window, cx);
        let color = self.row(
            crate::tr!("providers.accent").into_owned(),
            None,
            Some(accent),
            cx,
        );
        self.group(
            Some(crate::tr!("providers.harness.name").into_owned()),
            vec![name, color],
            cx,
        )
    }

    fn render_accent(
        &self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self
            .store
            .read(cx)
            .provider_profile_settings(profile_id)
            .accent_color;
        let mut swatches = h_flex().gap_2().items_center().flex_wrap();
        for (index, preset) in ACCENT_PRESETS.iter().enumerate() {
            let hex = (*preset).to_string();
            let selected = current
                .as_deref()
                .is_some_and(|current| current.eq_ignore_ascii_case(preset));
            let value = u32::from_str_radix(preset.trim_start_matches('#'), 16).unwrap_or(0);
            let label: SharedString = crate::tr!("providers.accent_select", color = hex.clone())
                .into_owned()
                .into();
            let profile = profile_id.to_owned();
            // The ring is always drawn, clear while unselected, so it can glide.
            let border = transition(
                ElementId::from(("accent-ring", index)),
                if selected {
                    cx.theme().foreground
                } else {
                    cx.theme().foreground.opacity(0.)
                },
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            // gpui-base's `ColorSwatch` keeps its focus handle to itself (no
            // `track_focus`), leaving only `focus_visible`, which switches at
            // once, to show the keyboard's focus. So the swatch is drawn as
            // one is — a radio button named for its colour, stating both
            // toggled and selected — on a clickable that holds its handle and
            // fades its ring (`motion::focus_ring`). Back to `ColorSwatch`
            // once it takes a focus handle.
            swatches = swatches.child(
                crate::material::accessible_clickable(
                    div(),
                    ("accent", index),
                    gpui::Role::RadioButton,
                    label.clone(),
                    cx,
                )
                .aria_toggled(if selected {
                    gpui::Toggled::True
                } else {
                    gpui::Toggled::False
                })
                .aria_selected(selected)
                // A swatch is its colour at a fixed size; there is no token for it.
                .size(px(22.))
                .rounded_full()
                .cursor_pointer()
                .bg(rgb(value))
                .border_2()
                .border_color(border)
                .with_tooltip(move |_, _| crate::widgets::tooltip::Tooltip::new(label.clone()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let color = (!selected).then(|| hex.clone());
                    this.update_profile(
                        profile.clone(),
                        ProfileSettingsPatch::SetAccentColor(color),
                        cx,
                    );
                })),
            );
        }
        swatches.into_any_element()
    }

    fn render_models(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = self.store.read(cx).provider_profile_settings(profile_id);
        let models = self.store.read(cx).provider_dialog_models(
            profile_id,
            &settings.custom_models,
            &settings.hidden_models,
        );
        let total = models.len();
        let folded = !self.models_open && total > MODELS_SHOWN + 1;
        let mut rows: Vec<AnyElement> = models
            .iter()
            .enumerate()
            .take(if folded { MODELS_SHOWN } else { total })
            .map(|(index, model)| {
                self.model_row(profile_id, index, model, &settings.hidden_models, cx)
            })
            .collect();
        if folded {
            rows.push(
                Button::new("harness-models-more")
                    .ghost()
                    .small()
                    .w_full()
                    .label(
                        crate::tr!(
                            "providers.harness.more_models",
                            count = total - MODELS_SHOWN
                        )
                        .into_owned(),
                    )
                    .icon(IconName::ChevronDown)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.models_open = true;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let input = self.input(Field::CustomModel, window, cx);
        rows.push(
            crate::material::group_row(cx)
                .child(div().flex_1().min_w_0().child(
                    Input::new(&input).placeholder(crate::tr!("providers.harness.model_id")),
                ))
                .child(
                    Button::new("harness-add-model")
                        .outline()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label(crate::tr!("providers.models.add"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_custom_model(window, cx)),
                        ),
                )
                .into_any_element(),
        );
        let mut group = v_flex()
            .w_full()
            .debug_selector(|| "harness-models".into())
            .child(self.group(
                Some(crate::tr!("providers.models.title").into_owned()),
                rows,
                cx,
            ));
        if let Some(error) =
            crate::motion::shown("slug-error", self.slug_error.clone(), SHOW, window, cx)
        {
            group = group.child(
                error.fold(
                    div()
                        .pt_1()
                        .pl(px(crate::material::CARD_INSET))
                        .text_secondary(false, cx)
                        .text_color(cx.theme().danger_foreground)
                        .child(error.value.clone()),
                ),
            );
        }
        group.into_any_element()
    }

    fn model_row(
        &self,
        profile_id: &str,
        index: usize,
        model: &ResolvedModel,
        hidden: &[String],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let capabilities = model
            .capabilities
            .iter()
            .copied()
            .map(model_capability_label)
            .collect::<Vec<_>>()
            .join(" · ");
        let store = self.store.clone();
        let kind = self.kind();
        let fav_profile = profile_id.to_owned();
        let fav_id = model.id.clone();
        let hide_profile = profile_id.to_owned();
        let hide_id = model.id.clone();
        let hidden = hidden.to_vec();
        let remove_profile = profile_id.to_owned();
        let remove_id = model.id.clone();
        crate::material::group_row(cx)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .items_center()
                    .text_body(false, cx)
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .when(model.hidden, |name| name.text_color(muted))
                            .child(crate::widgets::fade_text(model.name.clone())),
                    )
                    .when(!capabilities.is_empty(), |row| {
                        row.child(
                            div()
                                .id(("model-info", index))
                                .flex_none()
                                .child(Icon::new(IconName::Info).text_color(muted))
                                .with_tooltip(move |_, _| {
                                    crate::widgets::tooltip::Tooltip::new(capabilities.clone())
                                }),
                        )
                    })
                    .when(model.custom, |row| {
                        row.child(crate::material::chip(
                            crate::tr!("providers.models.custom").into_owned(),
                            cx,
                        ))
                    }),
            )
            .when(model.custom, |row| {
                row.child(
                    Button::new(("model-remove", index))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Delete)
                        .tooltip(crate::tr!("providers.models.remove"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let settings = this
                                .store
                                .read(cx)
                                .provider_profile_settings(&remove_profile);
                            let mut custom = settings.custom_models;
                            custom.retain(|id| *id != remove_id);
                            let mut hidden = settings.hidden_models;
                            hidden.retain(|id| *id != remove_id);
                            this.update_profile(
                                remove_profile.clone(),
                                ProfileSettingsPatch::SetCustomModels(custom),
                                cx,
                            );
                            this.update_profile(
                                remove_profile.clone(),
                                ProfileSettingsPatch::SetHiddenModels(hidden),
                                cx,
                            );
                        })),
                )
            })
            .child(
                Button::new(("model-fav", index))
                    .ghost()
                    .xsmall()
                    .icon(if model.favorite {
                        IconName::StarFill
                    } else {
                        IconName::Star
                    })
                    .tooltip(if model.favorite {
                        crate::tr!("providers.models.unfavorite")
                    } else {
                        crate::tr!("providers.models.favorite")
                    })
                    .on_click(move |_, _, cx| {
                        store.update(cx, |store, cx| {
                            store.toggle_favorite_model(
                                kind,
                                Some(fav_profile.clone()),
                                fav_id.clone(),
                                cx,
                            )
                        });
                    }),
            )
            .child(
                Switch::new(("model-shown", index))
                    .checked(!model.hidden)
                    .tooltip(crate::tr!("providers.harness.show_in_picker"))
                    .on_click(cx.listener(move |this, shown: &bool, _, cx| {
                        let mut hidden = hidden.clone();
                        hidden.retain(|id| *id != hide_id);
                        if !*shown {
                            hidden.push(hide_id.clone());
                        }
                        this.update_profile(
                            hide_profile.clone(),
                            ProfileSettingsPatch::SetHiddenModels(hidden),
                            cx,
                        );
                    })),
            )
            .into_any_element()
    }

    fn render_run(
        &mut self,
        description: &HarnessDescription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shared = self.lists_accounts(cx);
        let mut rows = Vec::new();
        rows.extend(self.default_permission_row(description, cx));
        if let Some(binary) = description.binary_name.clone() {
            rows.push(
                self.field_row(
                    Field::Binary,
                    crate::tr!("providers.harness.binary").into_owned(),
                    shared.then(|| crate::tr!("providers.harness.all_accounts").into_owned()),
                    Some(
                        crate::tr!("providers.harness.binary_placeholder", binary = binary)
                            .into_owned(),
                    ),
                    window,
                    cx,
                ),
            );
        }
        if !shared && let Some(variable) = description.config_dir_env.clone() {
            rows.push(self.field_row(
                Field::ConfigDir,
                crate::tr!("providers.harness.config_dir").into_owned(),
                Some(variable),
                description.default_config_dir.clone(),
                window,
                cx,
            ));
        }
        if description.launch_args {
            rows.push(self.field_row(
                Field::LaunchArgs,
                crate::tr!("providers.launch_args").into_owned(),
                shared.then(|| crate::tr!("providers.harness.all_accounts").into_owned()),
                None,
                window,
                cx,
            ));
        }
        for toggle in description.toggles.clone() {
            let enabled = self
                .store
                .read(cx)
                .provider_profile_settings(self.builtin_id())
                .toggle(&toggle);
            let builtin = self.builtin_id().to_owned();
            let id = toggle.clone();
            let selector = format!("harness-toggle-{toggle}");
            rows.push(
                div()
                    .w_full()
                    .debug_selector(move || selector.clone())
                    .child(
                        ToggleRow::new(
                            SharedString::from(format!("harness-toggle-{toggle}")),
                            crate::tr!(&format!("providers.toggle.{toggle}")).into_owned(),
                            enabled,
                            self.compact(),
                            cx.listener(move |this, checked: &bool, _, cx| {
                                this.update_profile(
                                    builtin.clone(),
                                    ProfileSettingsPatch::SetToggle {
                                        id: id.clone(),
                                        enabled: *checked,
                                    },
                                    cx,
                                )
                            }),
                        )
                        .description(
                            crate::tr!(&format!("providers.toggle.{toggle}_help")).into_owned(),
                        ),
                    )
                    .into_any_element(),
            );
        }
        let env = self.harness_env(window, cx);
        let unreadable = self.secrets_unreadable(cx);
        rows.push(
            v_flex()
                .w_full()
                .px(px(crate::material::CARD_INSET))
                .py(px(10.))
                .gap_1()
                .child(row_labels(
                    crate::tr!("providers.env.title").into_owned(),
                    shared.then(|| {
                        crate::tr!("providers.harness.all_accounts")
                            .into_owned()
                            .into()
                    }),
                    None,
                    self.compact(),
                    cx,
                ))
                .children(unreadable)
                .child(env)
                .into_any_element(),
        );
        v_flex()
            .w_full()
            .debug_selector(|| "harness-run".into())
            .child(self.group(
                Some(crate::tr!("providers.harness.run").into_owned()),
                rows,
                cx,
            ))
            .into_any_element()
    }

    /// What a new thread may do without asking, in the harness's own terms,
    /// where it has a choice and the thread's project names none.
    fn default_permission_row(
        &self,
        description: &HarnessDescription,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tcode_protocol::HarnessPermission {
            id,
            options,
            recommended,
        } = description.permission.clone()?;
        let builtin = self.builtin_id().to_owned();
        let chosen = self
            .store
            .read(cx)
            .provider_profile_settings(&builtin)
            .default_permission
            .clone();
        let current = chosen.clone().or(recommended);
        let selected = options
            .iter()
            .find(|option| Some(&option.value) == current.as_ref())
            .map(|option| crate::i18n::permission_option(&id, option).0)
            .unwrap_or_default();
        let options = options
            .iter()
            .map(|option| {
                let (label, description) = crate::i18n::permission_option(&id, option);
                crate::settings_rows::SelectRowOption {
                    value: option.value.clone(),
                    id: SharedString::from(format!("harness-permission-{}", option.value)),
                    label: label.into(),
                    description: description.map(Into::into),
                    selected: Some(&option.value) == current.as_ref(),
                }
            })
            .collect();
        let reset = crate::settings_rows::reset_action(
            "harness-permission-reset",
            chosen.is_some(),
            cx.listener({
                let builtin = builtin.clone();
                move |this, _, _, cx| {
                    this.update_profile(
                        builtin.clone(),
                        ProfileSettingsPatch::SetDefaultPermission(None),
                        cx,
                    )
                }
            }),
        );
        let dialog = cx.entity();
        Some(crate::settings_rows::select_row(
            "harness-permission",
            "harness-permission-popover",
            "harness-permission-menu",
            320.,
            crate::tr!("providers.harness.default_permission")
                .into_owned()
                .into(),
            Some(
                crate::tr!("providers.harness.default_permission_help")
                    .into_owned()
                    .into(),
            ),
            selected.into(),
            options,
            reset,
            crate::settings_rows::flip_row(&self.motion, "harness-permission", self.compact()),
            move |value, _, cx| {
                dialog.update(cx, |this, cx| {
                    this.update_profile(
                        builtin.clone(),
                        ProfileSettingsPatch::SetDefaultPermission(Some(value)),
                        cx,
                    )
                })
            },
            cx,
        ))
    }

    /// The servers the harness's own configuration lists, read-only: Tcode
    /// shows them and leaves them to the harness.
    fn render_native_mcp(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = match self.native_mcp.as_ref()? {
            Ok(servers) if servers.is_empty() => vec![self.row(
                crate::tr!("providers.harness.native_mcp_empty").into_owned(),
                None,
                None,
                cx,
            )],
            Ok(servers) => servers
                .iter()
                .map(|server| {
                    let selector = format!("harness-native-mcp-{}", server.name);
                    let target = if server.enabled {
                        server.target.clone()
                    } else {
                        crate::tr!("providers.harness.native_mcp_off", target = server.target)
                            .into_owned()
                    };
                    div()
                        .w_full()
                        .debug_selector(move || selector.clone())
                        .child(self.row(server.name.clone(), Some(target), None, cx))
                        .into_any_element()
                })
                .collect(),
            Err(error) => vec![self.row(
                crate::tr!("providers.harness.native_mcp_failed").into_owned(),
                Some(error.clone()),
                None,
                cx,
            )],
        };
        let name = harness_name(self.store.read(cx), &self.harness);
        Some(
            v_flex()
                .w_full()
                .debug_selector(|| "harness-native-mcp".into())
                .child(self.group(
                    Some(crate::tr!("providers.harness.native_mcp").into_owned()),
                    rows,
                    cx,
                ))
                .child(crate::settings_rows::footnote(
                    crate::tr!("providers.harness.native_mcp_help", name = name),
                    self.compact(),
                    cx,
                ))
                .into_any_element(),
        )
    }

    fn render_integrations(
        &self,
        description: &HarnessDescription,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let mut rows = Vec::new();
        let name = harness_name(self.store.read(cx), &self.harness);
        if description.plugins.manages() {
            let builtin = self.builtin_id().to_owned();
            rows.push(
                crate::material::accessible_clickable(
                    crate::material::group_row(cx),
                    "harness-plugins",
                    gpui::Role::Button,
                    crate::tr!("providers.harness.plugins").into_owned(),
                    cx,
                )
                .debug_selector(|| "harness-plugins".into())
                .cursor_pointer()
                .relative()
                .child(crate::material::row_hover(px(0.), cx))
                .child(row_labels(
                    crate::tr!("providers.harness.plugins").into_owned(),
                    Some(
                        crate::tr!("providers.harness.plugins_help")
                            .into_owned()
                            .into(),
                    ),
                    None,
                    self.compact(),
                    cx,
                ))
                .child(Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.manage_plugins(builtin.clone(), window, cx)
                }))
                .into_any_element(),
            );
        }
        if !description.mcp_servers {
            rows.push(self.row(
                crate::tr!("providers.harness.mcp").into_owned(),
                Some(crate::tr!("providers.mcp_unavailable").into_owned()),
                None,
                cx,
            ));
        }
        if description.native_computer_use {
            rows.push(self.row(
                crate::tr!("providers.harness.computer_use").into_owned(),
                Some(crate::tr!("providers.native_computer_use", name = name).into_owned()),
                None,
                cx,
            ));
        }
        (!rows.is_empty()).then(|| {
            self.group(
                Some(crate::tr!("providers.harness.integrations").into_owned()),
                rows,
                cx,
            )
        })
    }

    fn danger_row(
        &self,
        id: &'static str,
        title: String,
        body: String,
        action: String,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> AnyElement {
        crate::material::grouped(
            vec![
                crate::material::group_row(cx)
                    .child(row_labels(
                        title,
                        Some(body.into()),
                        None,
                        self.compact(),
                        cx,
                    ))
                    .child(
                        Button::new(id)
                            .debug_selector(move || id.into())
                            .danger()
                            .xsmall()
                            .label(action)
                            .on_click(on_click),
                    )
                    .into_any_element(),
            ],
            cx,
        )
        .into_any_element()
    }

    fn render_profile(
        &mut self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let Some(description) = describe(store, &self.harness) else {
            return crate::material::empty_row(crate::tr!("composer.loading"), cx)
                .into_any_element();
        };
        let builtin = profile_id == self.builtin_id();
        let endpoint = self.is_endpoint_profile(cx);
        let own = store.provider_profile_own_settings(&profile_id);
        let home = store.provider_profile_settings(&profile_id).home_path;
        let local = !store.is_remote();
        let mut column = v_flex().w_full().gap(crate::material::section_gap(cx));
        // What is wrong with this profile alone, in the CLI's or its own words.
        let trouble = store
            .provider_profile_snapshot(&profile_id)
            .filter(|snapshot| {
                matches!(
                    snapshot.status,
                    Some(ProviderStatusKind::Warning | ProviderStatusKind::Error)
                ) && !matches!(
                    snapshot.diagnostic,
                    Some(ProviderProbeDiagnostic::Unauthenticated)
                )
            })
            .map(|snapshot| crate::provider_status::snapshot_detail(&description, &snapshot))
            .filter(|detail| !detail.is_empty());
        if let Some(trouble) = trouble {
            column = column.child(
                crate::material::notice(cx.theme().warning, cx)
                    .debug_selector(|| "harness-banner-profile".into())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .child(trouble),
                    ),
            );
        }
        column = column.children(if endpoint {
            Some(self.render_endpoint(&description, window, cx))
        } else {
            self.render_sign_in(&description, &profile_id, window, cx)
        });
        column = column.child(self.render_identity(&profile_id, window, cx));
        if !endpoint {
            column = column.child(self.render_models(&profile_id, window, cx));
        }
        // Advanced: where the account keeps itself, and what it alone sets.
        let mut advanced = Vec::new();
        if let Some(variable) = description.config_dir_env.clone() {
            if builtin {
                advanced.push(self.field_row(
                    Field::ConfigDir,
                    crate::tr!("providers.harness.config_dir").into_owned(),
                    Some(variable),
                    description.default_config_dir.clone(),
                    window,
                    cx,
                ));
            } else if let Some(home) = home {
                let shown = home.display().to_string();
                advanced.push(self.row(
                    crate::tr!("providers.harness.config_dir").into_owned(),
                    Some(shown),
                    local.then(|| {
                        h_flex()
                            .w_full()
                            .justify_end()
                            .child(
                                Button::new("harness-reveal-home")
                                    .ghost()
                                    .xsmall()
                                    .label(crate::tr!("chat.reveal_in_file_manager"))
                                    .on_click(move |_, _, cx| cx.reveal_path(&home)),
                            )
                            .into_any_element()
                    }),
                    cx,
                ));
            }
        }
        if own.as_ref().is_some_and(|own| own.binary_path.is_some()) {
            advanced.push(self.field_row(
                Field::OwnBinary,
                crate::tr!("providers.harness.binary").into_owned(),
                Some(crate::tr!("providers.harness.own_override").into_owned()),
                None,
                window,
                cx,
            ));
        }
        if own.as_ref().is_some_and(|own| own.launch_args.is_some()) {
            advanced.push(self.field_row(
                Field::OwnArgs,
                crate::tr!("providers.launch_args").into_owned(),
                Some(crate::tr!("providers.harness.own_override").into_owned()),
                None,
                window,
                cx,
            ));
        }
        let env = self.profile_env(window, cx);
        let unreadable = self.secrets_unreadable(cx);
        advanced.push(
            v_flex()
                .w_full()
                .px(px(crate::material::CARD_INSET))
                .py(px(10.))
                .gap_1()
                .child(row_labels(
                    if endpoint {
                        crate::tr!("providers.harness.other_env").into_owned()
                    } else {
                        crate::tr!("providers.env.title").into_owned()
                    },
                    Some(
                        crate::tr!("providers.harness.this_account")
                            .into_owned()
                            .into(),
                    ),
                    None,
                    self.compact(),
                    cx,
                ))
                .children(unreadable)
                .child(env)
                .into_any_element(),
        );
        column = column.child(self.group(
            Some(crate::tr!("providers.harness.advanced").into_owned()),
            advanced,
            cx,
        ));
        if !builtin {
            let id = profile_id.clone();
            let (title, body) = if endpoint {
                (
                    crate::tr!("providers.harness.delete_endpoint").into_owned(),
                    crate::tr!("providers.harness.delete_endpoint_body").into_owned(),
                )
            } else {
                (
                    crate::tr!("providers.harness.delete_account").into_owned(),
                    crate::tr!("providers.harness.delete_account_body").into_owned(),
                )
            };
            column = column.child(self.danger_row(
                "harness-delete-profile",
                title,
                body,
                crate::tr!("providers.harness.delete").into_owned(),
                cx.listener(move |this, _, window, cx| this.confirm_delete(id.clone(), window, cx)),
                cx,
            ));
        }
        column.into_any_element()
    }

    fn render_add(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let endpoint_page = self.page == Page::AddEndpoint;
        let mut column = v_flex().w_full().gap_3();
        let presets = self.presets(cx);
        if endpoint_page {
            let mut tabs = TabBar::new("harness-presets").segmented();
            for (index, preset) in presets.iter().enumerate() {
                tabs = tabs.item(
                    TabItem::new(("harness-preset", index), preset.name.clone())
                        .selected(self.preset == Some(index))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.preset = Some(index);
                            this.seed_preset(window, cx);
                            cx.notify();
                        })),
                );
            }
            tabs = tabs.item(
                TabItem::new(
                    "harness-preset-custom",
                    crate::tr!("providers.harness.custom").into_owned(),
                )
                .selected(self.preset.is_none())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.preset = None;
                    this.seed_preset(window, cx);
                    cx.notify();
                })),
            );
            column = column.child(tabs).child(
                div().text_secondary(false, cx).child(
                    match self.preset.and_then(|index| presets.get(index)) {
                        Some(preset) => {
                            crate::tr!("providers.harness.preset_help", name = preset.name.clone())
                                .into_owned()
                        }
                        None => crate::tr!("providers.third_party.custom_help").into_owned(),
                    },
                ),
            );
        } else {
            column = column.child(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("providers.harness.add_account_help")),
            );
        }
        let mut rows = vec![self.field_row(
            Field::AddName,
            crate::tr!("providers.harness.name").into_owned(),
            None,
            (!endpoint_page).then(|| crate::tr!("providers.account.name_hint").into_owned()),
            window,
            cx,
        )];
        if endpoint_page {
            rows.push(self.field_row(
                Field::AddBaseUrl,
                crate::tr!("providers.third_party.base_url").into_owned(),
                None,
                None,
                window,
                cx,
            ));
            rows.push(self.field_row(
                Field::AddModel,
                crate::tr!("providers.third_party.model").into_owned(),
                None,
                None,
                window,
                cx,
            ));
            rows.push(self.field_row(
                Field::AddKey,
                crate::tr!("providers.third_party.key").into_owned(),
                None,
                Some(crate::tr!("providers.third_party.key_hint").into_owned()),
                window,
                cx,
            ));
        }
        rows.push(
            ToggleRow::new(
                "harness-inherit-config",
                crate::tr!("providers.inherit_config").into_owned(),
                self.inherit_config,
                self.compact(),
                cx.listener(|this, checked: &bool, _, cx| {
                    this.inherit_config = *checked;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
        column = column.child(crate::material::grouped(rows, cx));
        if let Some(error) = self.add_error.clone() {
            column = column.child(
                div()
                    .text_secondary(false, cx)
                    .text_color(cx.theme().danger_foreground)
                    .child(error),
            );
        }
        column
            .child(
                h_flex().w_full().justify_end().gap_2().child(
                    Button::new("harness-add")
                        .debug_selector(|| "harness-add".into())
                        .primary()
                        .small()
                        .loading(self.adding)
                        .label(if endpoint_page {
                            crate::tr!("providers.harness.add")
                        } else {
                            crate::tr!("providers.login.sign_in")
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                ),
            )
            .into_any_element()
    }
}

impl Render for HarnessDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Below this a label and a field side by side squeeze the label to a
        // word a line; the dialog takes most of the window.
        const STACKED_BELOW: f32 = 640.;
        self.narrow = self.embedded || window.viewport_size().width < px(STACKED_BELOW);
        let page = self.page.clone();
        let pages =
            crate::motion::CrossFade::new("harness-page", page.index(), 4, CHANGE, window, cx);
        let mut layers = Vec::new();
        for index in 0..4 {
            if !pages.shows(index) {
                continue;
            }
            let layer = match (&page, index) {
                // The overview depends on no page's state, so it fades out
                // as itself.
                (_, 0) => self.render_overview(window, cx),
                (Page::Profile(id), 1) => self.render_profile(id.clone(), window, cx),
                (Page::AddAccount | Page::AddEndpoint, 2 | 3) if index == page.index() => {
                    self.render_add(window, cx)
                }
                // Another page being left had its fields dropped with it.
                _ => div().into_any_element(),
            };
            layers.push((index, layer));
        }
        let title = self.embedded.then(|| self.render_title(cx));
        let body = v_flex()
            .w_full()
            .debug_selector(|| "harness-dialog-form".into())
            .gap(crate::material::section_gap(cx))
            .children(title)
            .child(pages.render(layers).w_full());
        if self.embedded {
            return div()
                .w_full()
                .track_focus(&cx.focus_handle())
                .child(body)
                .into_any_element();
        }
        div()
            .id("harness-dialog-body")
            .debug_selector(|| "harness-dialog-body".into())
            .track_focus(&cx.focus_handle())
            .size_full()
            .overflow_y_scroll_area()
            .child(body)
            .into_any_element()
    }
}

/// The confirmation of deleting a profile: its name, what goes with it, and
/// how many threads move to the default account.
fn delete_confirmation(
    store: &WorkspaceStore,
    profile_id: &str,
    kind: ProviderKind,
    endpoint: bool,
) -> (String, String) {
    let name = store.provider_profile_display_name(profile_id);
    let default_name = store.provider_profile_display_name(Settings::builtin_profile_id(kind));
    let mut body = if endpoint {
        crate::tr!("providers.harness.delete_endpoint_body").into_owned()
    } else {
        crate::tr!("providers.harness.delete_account_body").into_owned()
    };
    let threads = store.threads_using_profile(profile_id);
    if threads > 0 {
        let moved = if threads == 1 {
            crate::tr!("providers.harness.delete_threads_one", name = default_name)
        } else {
            crate::tr!(
                "providers.harness.delete_threads",
                count = threads,
                name = default_name
            )
        };
        body = format!("{body} {moved}");
    }
    (
        crate::tr!("providers.delete_confirm_title", name = name).into_owned(),
        body,
    )
}

/// Turn every profile of a harness on or off at once.
pub(crate) fn set_harness_enabled(
    store: &Entity<WorkspaceStore>,
    harness: &HarnessId,
    enabled: bool,
    cx: &mut App,
) {
    store.update(cx, |store, _| match harness {
        HarnessId::Native(kind) => {
            store.update_profile_settings(
                Settings::builtin_profile_id(*kind).to_owned(),
                ProfileSettingsPatch::SetHarnessEnabled { enabled },
            );
            store.reload_provider();
        }
        HarnessId::Acp(id) => store.update_acp_agent(
            id.clone(),
            tcode_core::acp::AcpAgentPatch::SetEnabled { enabled },
        ),
    });
}

fn endpoint_chip(cx: &App) -> AnyElement {
    crate::material::semantic_chip(
        crate::tr!("providers.harness.endpoint_profile").into_owned(),
        cx.theme().info.opacity(0.12),
        cx.theme().info_foreground,
        cx,
    )
    .into_any_element()
}

/// An endpoint profile's second line: the endpoint's host and its model.
fn endpoint_summary(description: &HarnessDescription, env: &[EnvVar]) -> String {
    let Some(endpoint) = &description.endpoint else {
        return String::new();
    };
    let host = env_value(env, &endpoint.base_url_env).map(|url| {
        let rest = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
        rest.split('/').next().unwrap_or(rest).to_owned()
    });
    let model = endpoint
        .model_env
        .as_deref()
        .and_then(|name| env_value(env, name));
    [host, model]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// An account's second line: its plan and its email, hidden, or that it is
/// signed out.
fn account_summary(snapshot: Option<&ProviderSnapshot>) -> String {
    let Some(snapshot) =
        snapshot.filter(|snapshot| !snapshot.checking && snapshot.status.is_some())
    else {
        return crate::tr!("providers.status.checking").into_owned();
    };
    match snapshot.auth.as_ref() {
        Some(auth) if auth.status == AuthStatus::Authenticated => {
            let plan = auth.label.clone().filter(|label| !label.is_empty());
            let email = auth
                .email
                .as_deref()
                .filter(|email| !email.is_empty())
                .map(redact_email);
            [plan, email]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ")
        }
        Some(auth) if auth.status == AuthStatus::Unauthenticated => {
            crate::tr!("providers.harness.signed_out").into_owned()
        }
        _ => String::new(),
    }
}

/// The names of a harness's endpoint variables.
fn endpoint_names(description: &HarnessDescription) -> Vec<String> {
    description
        .endpoint
        .iter()
        .flat_map(|endpoint| {
            [
                Some(endpoint.base_url_env.clone()),
                Some(endpoint.key_env.clone()),
                endpoint.model_env.clone(),
            ]
        })
        .flatten()
        .collect()
}

fn env_value(env: &[EnvVar], name: &str) -> Option<String> {
    env.iter()
        .rev()
        .find(|var| var.name == name && !var.sensitive)
        .map(|var| var.value.clone())
}

/// Set `name` to `value` where it is, or add it; `None` removes it.
fn set_env_value(env: &mut Vec<EnvVar>, name: &str, value: Option<String>) {
    match value {
        Some(value) => match env.iter_mut().find(|var| var.name == name) {
            Some(var) => {
                var.value = value;
                var.sensitive = false;
            }
            None => env.push(EnvVar {
                name: name.to_owned(),
                value,
                sensitive: false,
            }),
        },
        None => env.retain(|var| var.name != name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext, size};
    use tcode_core::provider_status::ProviderAuth;
    use tcode_core::settings::{ProviderProfile, ProviderSettings};
    use tcode_protocol::{
        ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ProviderVersionStatus,
        ProvidersStatus, ServerEvent, Topic, decode_client_line, encode_line,
    };

    /// A client attached to a host that holds `settings` and reports
    /// `providers` and `sessions`, as the host's messages would.
    struct Host {
        store: Entity<WorkspaceStore>,
        outgoing: async_channel::Receiver<String>,
        incoming: async_channel::Sender<String>,
        _pump: gpui::Task<()>,
    }

    impl Host {
        fn new(
            settings: Settings,
            providers: ProvidersStatus,
            sessions: Vec<tcode_core::project::SessionMeta>,
            cx: &mut TestAppContext,
        ) -> Self {
            cx.update(crate::test_client::init);
            let (to_host, outgoing) = async_channel::unbounded();
            let (incoming, from_host) = async_channel::unbounded();
            let event = |topic, event| {
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap()
            };
            let deferred = vec![
                event(Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
                event(Topic::Providers, ServerEvent::ProvidersReplaced(providers)),
                event(
                    Topic::Index,
                    ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions,
                        projects: Vec::new(),
                    }),
                ),
            ];
            let link = tcode_client::HostLink::new(to_host, from_host);
            let pump_link = link.clone();
            let pump = cx.background_executor.spawn(async move {
                pump_link.pump().await;
            });
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    link,
                    crate::store::WorkspaceAttachment::Local,
                    None,
                    None,
                    cx,
                )
            });
            crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
            cx.run_until_parked();
            cx.run_until_parked();
            Self {
                store,
                outgoing,
                incoming,
                _pump: pump,
            }
        }

        /// The settings edits the client sent, in order.
        fn sent_patches(&self) -> Vec<(String, ProfileSettingsPatch)> {
            std::iter::from_fn(|| self.outgoing.try_recv().ok())
                .filter_map(|line| match decode_client_line(&line).ok()?.payload {
                    ClientPayload::Command(Command::UpdateProfileSettings {
                        profile_id,
                        patch,
                    }) => Some((profile_id, patch)),
                    _ => None,
                })
                .collect()
        }

        fn open<'a>(
            &self,
            harness: HarnessId,
            cx: &'a mut TestAppContext,
        ) -> (Entity<HarnessDialog>, &'a mut VisualTestContext) {
            let store = self.store.clone();
            let (dialog, cx) = cx
                .add_window_view(|window, cx| HarnessDialog::new(store, harness, true, window, cx));
            cx.simulate_resize(size(px(900.), px(2400.)));
            draw(cx);
            (dialog, cx)
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    fn ready(email: Option<&str>, label: Option<&str>) -> ProviderSnapshot {
        ProviderSnapshot {
            checked_at: Some(1),
            installed: true,
            version: Some("2.1.318".into()),
            status: Some(ProviderStatusKind::Ready),
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: label.map(Into::into),
                email: email.map(Into::into),
            }),
            ..ProviderSnapshot::default()
        }
    }

    /// Claude Code with a second account, pi, Codex and an ACP agent, every
    /// harness described by the host and every profile probed ready.
    fn settings_and_status() -> (Settings, ProvidersStatus) {
        let mut settings = Settings::default();
        settings.profiles.insert(
            "work".into(),
            ProviderProfile {
                kind: ProviderKind::ClaudeCode,
                settings: ProviderSettings {
                    display_name: Some("Work".into()),
                    ..ProviderSettings::default()
                },
            },
        );
        settings.acp_agents.insert(
            "gemini".into(),
            tcode_core::acp::InstalledAcpAgent {
                id: "gemini".into(),
                name: "Gemini CLI".into(),
                version: "0.31.0".into(),
                icon: None,
                launch: agent::AcpLaunch::Npx {
                    package: "@google/gemini-cli".into(),
                    args: vec!["--experimental-acp".into()],
                    env: Vec::new(),
                },
                enabled: true,
                env: Vec::new(),
                launch_args: None,
            },
        );
        let mut status = ProvidersStatus {
            harnesses: ProviderKind::NATIVE
                .into_iter()
                .map(HarnessDescription::native)
                .chain([HarnessDescription {
                    acp_agent_id: Some("gemini".into()),
                    launch_summary: Some("npx @google/gemini-cli --experimental-acp".into()),
                    launch_args: true,
                    mcp_servers: true,
                    ..HarnessDescription::native(ProviderKind::Acp)
                }])
                .collect(),
            ..ProvidersStatus::default()
        };
        for id in [
            "claude",
            "work",
            "pi",
            "codex",
            "opencode",
            "native:cursor",
            "native:grok",
        ] {
            status.provider_snapshots.insert(
                id.into(),
                ready(
                    (id == "claude").then_some("user@example.com"),
                    (id == "claude").then_some("Claude Max Subscription"),
                ),
            );
        }
        (settings, status)
    }

    /// Which sections a harness's settings have comes from what the host says
    /// the harness is: accounts listed where they can be added, one account's
    /// sign-in and models on the overview otherwise, a switch where the
    /// harness declares one, and an ACP agent with neither accounts nor
    /// models.
    #[gpui::test]
    fn sections_follow_the_harness_description(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (settings, status) = settings_and_status();
        let host = Host::new(settings, status, Vec::new(), cx);
        assert_eq!(
            host.store
                .read_with(cx, |store, _| store.provider_profile_display_name("claude")),
            "Claude Code (user)",
            "beside a second account, the default one is named for who it is"
        );
        let shown = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.debug_bounds(selector).is_some()
        };
        for (harness, present, absent) in [
            (
                HarnessId::Native(ProviderKind::ClaudeCode),
                &[
                    "harness-accounts",
                    "harness-account-work",
                    "harness-add-account",
                    "harness-add-endpoint",
                    "harness-field-Binary",
                    "harness-field-LaunchArgs",
                    "harness-plugins",
                    "harness-permission",
                ][..],
                &[
                    "harness-models",
                    "harness-field-ConfigDir",
                    "harness-remove-agent",
                ][..],
            ),
            (
                HarnessId::Native(ProviderKind::Codex),
                &[
                    "harness-accounts",
                    "harness-add-account",
                    "harness-plugins",
                    "harness-permission",
                ][..],
                &["harness-add-endpoint", "harness-field-LaunchArgs"][..],
            ),
            (
                HarnessId::Native(ProviderKind::Pi),
                &[
                    "harness-models",
                    "harness-field-ConfigDir",
                    "harness-toggle-trust_project_extensions",
                    "harness-plugins",
                ][..],
                &["harness-accounts", "harness-permission"][..],
            ),
            (
                HarnessId::Acp("gemini".into()),
                &["harness-field-LaunchArgs", "harness-remove-agent"][..],
                &[
                    "harness-accounts",
                    "harness-models",
                    "harness-field-Binary",
                    "harness-plugins",
                    "harness-permission",
                ][..],
            ),
        ] {
            let (_dialog, cx) = host.open(harness.clone(), cx);
            for selector in present {
                assert!(shown(cx, selector), "{harness:?} must show {selector}");
            }
            for selector in absent {
                assert!(!shown(cx, selector), "{harness:?} must not show {selector}");
            }
        }
    }

    /// Secrets the host could not read are not shown as none saved: the
    /// variables say why.
    #[gpui::test]
    fn unreadable_secrets_are_said_beside_the_variables(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (settings, mut status) = settings_and_status();
        status.secrets_unreadable = Some("secrets.json is unreadable".into());
        let host = Host::new(settings, status, Vec::new(), cx);
        let (_dialog, cx) = host.open(HarnessId::Native(ProviderKind::Pi), cx);
        assert!(cx.debug_bounds("harness-secrets-unreadable").is_some());
    }

    /// The dialog lists what the host read from the harness's own files and
    /// offers nothing to change there; the default permission it does offer
    /// goes to the host as the harness's patch.
    #[gpui::test]
    fn a_harness_shows_its_own_mcp_servers_and_saves_its_default_permission(
        cx: &mut TestAppContext,
    ) {
        use tcode_protocol::{ClientPayload, Command, Query, QueryResponse, decode_client_line};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (settings, status) = settings_and_status();
        let host = Host::new(settings, status, Vec::new(), cx);
        let outgoing = host.outgoing.clone();
        let incoming = host.incoming.clone();
        let sent = move || -> Vec<(u64, ClientPayload)> {
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .map(|line| decode_client_line(&line).unwrap())
                .map(|message| (message.id, message.payload))
                .collect()
        };
        sent();
        let (_dialog, cx) = host.open(HarnessId::Native(ProviderKind::ClaudeCode), cx);
        assert!(cx.debug_bounds("harness-native-mcp").is_none());
        let id = sent()
            .into_iter()
            .find_map(|(id, payload)| match payload {
                ClientPayload::Query(Query::NativeMcpServers { profile_id }) => {
                    assert_eq!(profile_id, "claude");
                    Some(id)
                }
                _ => None,
            })
            .expect("opening asks the host");
        incoming
            .try_send(
                encode_line(&HostMessage::QueryResult {
                    id,
                    result: Ok(QueryResponse::NativeMcpServers(vec![
                        agent::NativeMcpServer {
                            name: "linear".into(),
                            remote: true,
                            target: "https://mcp.linear.app".into(),
                            enabled: true,
                        },
                    ])),
                })
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("harness-native-mcp-linear").is_some());

        let trigger = cx.debug_bounds("harness-permission").unwrap();
        cx.simulate_click(trigger.center(), Default::default());
        crate::test_frames::draw_settled(cx);
        let option = cx.debug_bounds("harness-permission-acceptEdits").unwrap();
        cx.simulate_click(option.center(), Default::default());
        cx.run_until_parked();
        let saved = sent().into_iter().any(|(_, payload)| {
            matches!(
                payload,
                ClientPayload::Command(Command::UpdateProfileSettings { profile_id, patch })
                    if profile_id == "claude"
                        && patch
                            == ProfileSettingsPatch::SetDefaultPermission(Some(
                                "acceptEdits".into()
                            ))
            )
        });
        assert!(saved, "the choice reaches the host");
    }

    /// What is wrong with a harness is said at the top of its settings: off,
    /// failing to run, older than Tcode supports, out of date; and a harness
    /// whose CLI is missing shows only how to install it.
    #[gpui::test]
    fn states_show_as_banners_and_a_missing_cli_as_its_install(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (mut settings, mut status) = settings_and_status();
        settings.provider_mut(ProviderKind::ClaudeCode).enabled = false;
        settings.profiles.get_mut("work").unwrap().settings.enabled = false;
        status.provider_snapshots.insert(
            "pi".into(),
            ProviderSnapshot {
                checked_at: Some(1),
                installed: true,
                status: Some(ProviderStatusKind::Error),
                diagnostic: Some(ProviderProbeDiagnostic::FailedCli),
                message: Some("exit status: 1: Error: Cannot find module 'node:sqlite'".into()),
                ..ProviderSnapshot::default()
            },
        );
        status.provider_snapshots.insert(
            "native:cursor".into(),
            ProviderSnapshot {
                checked_at: Some(1),
                installed: false,
                status: Some(ProviderStatusKind::Error),
                diagnostic: Some(ProviderProbeDiagnostic::MissingCli),
                ..ProviderSnapshot::default()
            },
        );
        status.provider_snapshots.insert(
            "opencode".into(),
            ProviderSnapshot {
                version: Some("1.19.5".into()),
                supported_from: Some("2.0.6".into()),
                ..ready(None, None)
            },
        );
        status.provider_versions.insert(
            ProviderKind::Codex,
            ProviderVersionStatus {
                installed: Some("0.166.0".into()),
                latest: Some("0.167.0".into()),
                update_available: true,
                update_command: Some("npm install -g @openai/codex".into()),
                ..ProviderVersionStatus::default()
            },
        );
        let host = Host::new(settings, status, Vec::new(), cx);
        for (harness, present, absent) in [
            (
                ProviderKind::ClaudeCode,
                &["harness-banner-disabled", "harness-run"][..],
                &["harness-banner-failed", "harness-install"][..],
            ),
            (
                ProviderKind::Pi,
                &["harness-banner-failed"][..],
                &["harness-banner-disabled"][..],
            ),
            (
                ProviderKind::Codex,
                &["harness-banner-update"][..],
                &["harness-banner-disabled", "harness-banner-outdated"][..],
            ),
            (
                ProviderKind::OpenCode,
                &["harness-banner-outdated"][..],
                &["harness-banner-failed", "harness-install"][..],
            ),
            (
                ProviderKind::Cursor,
                &["harness-install"][..],
                &["harness-run", "harness-models", "harness-accounts"][..],
            ),
        ] {
            let (_dialog, cx) = host.open(HarnessId::Native(harness), cx);
            for selector in present {
                assert!(
                    cx.debug_bounds(selector).is_some(),
                    "{harness:?}: {selector}"
                );
            }
            for selector in absent {
                assert!(
                    cx.debug_bounds(selector).is_none(),
                    "{harness:?}: no {selector}"
                );
            }
        }
    }

    /// A field is saved the moment it is left, with no Save to press: the
    /// harness's binary as its own, an account's name as that account's.
    #[gpui::test]
    fn fields_are_saved_as_they_are_left(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (settings, status) = settings_and_status();
        let host = Host::new(settings, status, Vec::new(), cx);
        let (_, cx) = host.open(HarnessId::Native(ProviderKind::ClaudeCode), cx);
        host.sent_patches();

        let field = cx.debug_bounds("harness-field-Binary").unwrap();
        cx.simulate_click(field.center(), Modifiers::default());
        cx.simulate_input("/opt/claude");
        cx.simulate_keystrokes("enter");
        draw(cx);

        let row = cx.debug_bounds("harness-account-work").unwrap();
        cx.simulate_click(row.center(), Modifiers::default());
        draw(cx);
        let name = cx.debug_bounds("harness-field-Name").unwrap();
        cx.simulate_click(name.center(), Modifiers::default());
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input("Office");
        // Leaving the field by clicking elsewhere saves it too.
        let back = cx.debug_bounds("harness-back").unwrap();
        cx.simulate_click(back.center(), Modifiers::default());
        draw(cx);

        assert_eq!(
            host.sent_patches(),
            vec![
                (
                    "claude".into(),
                    ProfileSettingsPatch::SetBinaryPath(Some("/opt/claude".into()))
                ),
                (
                    "work".into(),
                    ProfileSettingsPatch::SetDisplayName(Some("Office".into()))
                ),
            ]
        );
    }

    /// Deleting an account says how many threads still run on it and which
    /// account they move to.
    #[gpui::test]
    fn deleting_an_account_counts_the_threads_that_move(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (settings, status) = settings_and_status();
        let thread = |profile: Option<&str>| {
            let mut meta = tcode_core::project::SessionMeta::new(
                ProviderKind::ClaudeCode,
                "/repo".into(),
                None,
            );
            meta.profile_id = profile.map(Into::into);
            meta
        };
        let sessions = vec![thread(Some("work")), thread(Some("work")), thread(None)];
        let host = Host::new(settings, status, sessions, cx);
        let (title, body) = host.store.read_with(cx, |store, _| {
            delete_confirmation(store, "work", ProviderKind::ClaudeCode, false)
        });
        assert_eq!(title, "Delete Claude Code (Work)?");
        assert_eq!(
            body,
            "Also deletes the config directory Tcode created for it, with its sessions and \
             sign-in. The 2 threads still using it switch to Claude Code (user)."
        );
    }
}
