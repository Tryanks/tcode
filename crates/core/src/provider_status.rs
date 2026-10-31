//! Pure provider status data and semantic card-summary derivation.

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
}

/// Where a sign-in the host runs through the provider CLI stands.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ProviderLogin {
    Starting,
    /// The CLI printed its sign-in page and waits for the code that page shows.
    AwaitingCode {
        url: String,
    },
    Finishing,
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDot {
    Loading,
    Success,
    Warning,
    Error,
    Amber,
}
