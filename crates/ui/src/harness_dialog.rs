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

mod accounts;
mod actions;
mod banners;
mod pages;
mod profile;
mod run;

#[cfg(test)]
mod tests;
