//! One Settings → Harnesses row per harness, and the glyphs harnesses and
//! their profiles are drawn with.
//!
//! A row says only what its title cannot: how many accounts the harness
//! has, the plan its one account is on, that it is off or an ACP agent. A
//! chip appears only when something is wrong. The whole row opens the
//! harness's settings; its switch turns the harness on or off.

use crate::icon::Icon;
use crate::{
    harness_dialog::{HarnessId, describe, harness_name, harness_profiles, set_harness_enabled},
    provider_status::ProviderSnapshot,
    store::WorkspaceStore,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::TooltipTrigger as _,
    widgets::switch::Switch,
};
use gpui::{
    AnyElement, App, ClickEvent, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, rgb,
};
use gpui_base::{h_flex, v_flex};

use agent::ProviderKind;
use tcode_core::provider_status::{AuthStatus, ProviderProbeDiagnostic, ProviderStatusKind};
use tcode_core::settings::{ResolvedProfile, builtin_provider_color};

/// What is wrong with a harness, as its row's chip says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trouble {
    NotInstalled,
    Failed,
    SignedOut,
    NeedsAttention,
    /// Older than the oldest release Tcode supports.
    Outdated,
}

/// A harness row's second line and its chip, from the host's description
/// of the harness and what was probed of each of its profiles.
pub(crate) fn row_summary(
    harness: &HarnessId,
    profiles: &[(ResolvedProfile, Option<ProviderSnapshot>)],
    enabled: bool,
) -> (String, Option<Trouble>) {
    if matches!(harness, HarnessId::Acp(_)) {
        let line = if enabled {
            "ACP".to_owned()
        } else {
            crate::tr!("providers.status.disabled").into_owned()
        };
        return (line, None);
    }
    let builtin = profiles.first().and_then(|(_, snapshot)| snapshot.as_ref());
    let Some(builtin) = builtin.filter(|snapshot| !snapshot.checking && snapshot.status.is_some())
    else {
        return (crate::tr!("providers.status.checking").into_owned(), None);
    };
    let trouble = if !builtin.installed {
        Some(Trouble::NotInstalled)
    } else if builtin.diagnostic == Some(ProviderProbeDiagnostic::FailedCli) {
        Some(Trouble::Failed)
    } else {
        profiles
            .iter()
            .filter_map(|(_, snapshot)| snapshot.as_ref())
            .find_map(|snapshot| match (snapshot.diagnostic, snapshot.status) {
                (Some(ProviderProbeDiagnostic::Unauthenticated), _) => Some(Trouble::SignedOut),
                (
                    Some(
                        ProviderProbeDiagnostic::MissingApiKey
                        | ProviderProbeDiagnostic::SharedCredentials
                        | ProviderProbeDiagnostic::IndeterminateAuth,
                    ),
                    _,
                )
                | (_, Some(ProviderStatusKind::Warning | ProviderStatusKind::Error)) => {
                    Some(Trouble::NeedsAttention)
                }
                _ => None,
            })
            .or_else(|| {
                builtin
                    .supported_from
                    .is_some()
                    .then_some(Trouble::Outdated)
            })
    };
    let line = if !enabled {
        crate::tr!("providers.status.disabled").into_owned()
    } else if profiles.len() > 1 {
        crate::tr!("providers.harness.profile_count", count = profiles.len()).into_owned()
    } else {
        builtin
            .auth
            .as_ref()
            .filter(|auth| auth.status == AuthStatus::Authenticated)
            .and_then(|auth| auth.label.clone())
            .unwrap_or_default()
    };
    (line, trouble)
}

/// The row of `harness` in Settings → Harnesses.
pub(crate) fn harness_row(
    store: &Entity<WorkspaceStore>,
    harness: &HarnessId,
    on_open: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let state = store.read(cx);
    let name = harness_name(state, harness);
    let profiles: Vec<(ResolvedProfile, Option<ProviderSnapshot>)> =
        harness_profiles(state, harness)
            .into_iter()
            .map(|profile| {
                let snapshot = state.provider_profile_snapshot(&profile.id);
                (profile, snapshot)
            })
            .collect();
    let (enabled, version, update) = match harness {
        HarnessId::Native(kind) => {
            let versions = state.provider_version_status(*kind);
            (
                profiles.iter().any(|(profile, _)| profile.settings.enabled),
                profiles
                    .first()
                    .and_then(|(_, snapshot)| snapshot.as_ref()?.version.clone())
                    .or_else(|| versions.as_ref().and_then(|v| v.installed.clone())),
                versions.is_some_and(|v| v.update_available),
            )
        }
        HarnessId::Acp(id) => {
            let agent = state.installed_acp_agent(id);
            (
                agent.as_ref().is_some_and(|agent| agent.enabled),
                agent.map(|agent| agent.version).filter(|v| !v.is_empty()),
                describe(state, harness)
                    .is_some_and(|description| description.registry_update.is_some()),
            )
        }
    };
    let (line, trouble) = row_summary(harness, &profiles, enabled);
    let glyph = provider_glyph(harness.kind());
    let key = harness.key();
    let chip = trouble.map(|trouble| {
        let theme = cx.theme();
        let (label, bg, fg) = match trouble {
            Trouble::NotInstalled => (
                "providers.harness.not_installed",
                theme.danger,
                theme.danger_foreground,
            ),
            Trouble::Failed => (
                "providers.harness.failed",
                theme.danger,
                theme.danger_foreground,
            ),
            Trouble::SignedOut => (
                "providers.harness.signed_out",
                theme.warning,
                theme.warning_foreground,
            ),
            Trouble::NeedsAttention => (
                "providers.status.needs_attention",
                theme.warning,
                theme.warning_foreground,
            ),
            Trouble::Outdated => (
                "providers.harness.outdated",
                theme.warning,
                theme.warning_foreground,
            ),
        };
        crate::material::semantic_chip(crate::tr!(label).into_owned(), bg.opacity(0.12), fg, cx)
            .flex_none()
    });
    let store = store.clone();
    let toggled = harness.clone();
    let body = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("harness-row-{key}")),
        gpui::Role::Button,
        crate::tr!("providers.configure", name = name.clone()),
        cx,
    )
    .debug_selector({
        let key = key.clone();
        move || format!("harness-row-{key}")
    })
    .flex_1()
    .min_w_0()
    .gap_3()
    .items_center()
    .cursor_pointer()
    .child(div().flex_none().text_body(false, cx).child(glyph))
    .child(
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_title(cx)
                            .child(name.clone()),
                    )
                    .children(version.map(|version| {
                        div()
                            .flex_none()
                            .text_meta_mono(false, cx)
                            .child(format!("v{}", version.trim_start_matches('v')))
                    })),
            )
            .when(!line.is_empty(), |column| {
                column.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_secondary(false, cx)
                        .child(line),
                )
            }),
    )
    .children(chip)
    .when(update, |row| {
        row.child(
            div()
                .id(SharedString::from(format!("harness-update-{key}")))
                .flex_none()
                .child(
                    Icon::empty()
                        .path("icons/download.svg")
                        .text_color(cx.theme().info),
                )
                .with_tooltip(|_, _| {
                    crate::widgets::tooltip::Tooltip::new(
                        crate::tr!("providers.update_title").into_owned(),
                    )
                }),
        )
    })
    .on_click(on_open);
    crate::material::group_row(cx)
        .child(body)
        .child(
            Switch::new(SharedString::from(format!("harness-enable-{key}")))
                .checked(enabled)
                .tooltip(crate::tr!("providers.enable", name = name))
                .on_click(move |checked: &bool, _, cx| {
                    set_harness_enabled(&store, &toggled, *checked, cx)
                }),
        )
        .into_any_element()
}

/// The provider's glyph; every ACP agent shares one. Claude's is pre-tinted
/// with its brand color; the others take the text color.
pub fn provider_glyph(provider: ProviderKind) -> Icon {
    let (path, tint) = match provider {
        ProviderKind::ClaudeCode => ("icons/claude.svg", builtin_provider_color(provider)),
        ProviderKind::Codex => ("icons/openai.svg", None),
        ProviderKind::Pi => ("icons/pi.svg", None),
        ProviderKind::OpenCode => ("icons/opencode.svg", None),
        ProviderKind::Cursor => ("icons/cursor.svg", None),
        ProviderKind::Grok => ("icons/grok.svg", None),
        ProviderKind::Acp => ("icons/box.svg", None),
    };
    Icon::empty()
        .path(path)
        .when_some(tint, |icon, tint| icon.text_color(rgb(tint)))
}

/// The glyph a provider profile is drawn with wherever that profile, rather
/// than its CLI, is meant: the kind's [`provider_glyph`] in the profile's color,
/// the one its sidebar threads are marked with. `None` is the kind's built-in
/// profile. An ACP agent has no profile, so its glyph keeps the text color.
pub fn profile_glyph(
    store: &WorkspaceStore,
    provider: ProviderKind,
    profile_id: Option<&str>,
) -> Icon {
    if provider == ProviderKind::Acp {
        return provider_glyph(provider);
    }
    let profile_id =
        profile_id.unwrap_or_else(|| tcode_core::settings::Settings::builtin_profile_id(provider));
    provider_glyph(provider).text_color(rgb(store.provider_profile_color(profile_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_core::provider_status::ProviderAuth;

    /// A harness whose CLI is older than Tcode supports says so on its row
    /// when nothing more pressing is wrong; a signed-out account still is.
    #[test]
    fn an_older_cli_marks_its_row_unless_something_else_is_wrong() {
        let settings = tcode_core::settings::Settings::default();
        let profile = settings
            .resolved_profile(tcode_core::settings::Settings::builtin_profile_id(
                ProviderKind::Codex,
            ))
            .unwrap();
        let probed = |auth: AuthStatus| ProviderSnapshot {
            checked_at: Some(1),
            installed: true,
            version: Some("0.140.2".into()),
            status: Some(ProviderStatusKind::Ready),
            auth: Some(ProviderAuth {
                status: auth,
                label: None,
                email: None,
            }),
            supported_from: Some("0.148.0".into()),
            ..ProviderSnapshot::default()
        };
        let trouble = |snapshot: ProviderSnapshot| {
            row_summary(
                &HarnessId::Native(ProviderKind::Codex),
                &[(profile.clone(), Some(snapshot))],
                true,
            )
            .1
        };
        assert_eq!(
            trouble(probed(AuthStatus::Authenticated)),
            Some(Trouble::Outdated)
        );
        assert_eq!(
            trouble(ProviderSnapshot {
                diagnostic: Some(ProviderProbeDiagnostic::Unauthenticated),
                ..probed(AuthStatus::Unauthenticated)
            }),
            Some(Trouble::SignedOut)
        );
    }
}
