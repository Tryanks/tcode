//! Pure provider status data: what a probe and a sign-in found.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderStatusKind {
    Ready,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AuthStatus {
    Authenticated,
    Unauthenticated,
    Unknown,
}

/// A presentation-free explanation for a provider probe outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderProbeDiagnostic {
    MissingCli,
    FailedCli,
    Unauthenticated,
    IndeterminateAuth,
    /// The CLI keeps one sign-in for every config directory, so this
    /// profile's account login is shared with the other account profiles.
    SharedCredentials,
    /// An endpoint profile signs in with an API key it has none of.
    MissingApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderAuth {
    pub status: AuthStatus,
    pub label: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderSnapshot {
    pub checked_at: Option<u64>,
    pub installed: bool,
    pub version: Option<String>,
    pub status: Option<ProviderStatusKind>,
    pub auth: Option<ProviderAuth>,
    pub diagnostic: Option<ProviderProbeDiagnostic>,
    pub message: Option<String>,
    pub checking: bool,
    #[serde(default)]
    pub login: Option<ProviderLogin>,
    /// The CLI the probe ran, as the host resolved it.
    #[serde(default)]
    pub binary: Option<String>,
    /// The oldest release Tcode supports, present when `version` is older.
    /// The CLI still runs; it may not work correctly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_from: Option<String>,
}

/// Where a sign-in the host runs through the provider CLI stands.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ProviderLogin {
    Starting,
    /// The CLI waits for the code its sign-in page shows; `url` is that page
    /// when the CLI printed one.
    AwaitingCode {
        url: Option<String>,
    },
    /// The CLI waits for the user to enter `code` on its sign-in page.
    AwaitingDevice {
        url: Option<String>,
        code: String,
    },
    Finishing,
    Failed {
        message: String,
    },
}
