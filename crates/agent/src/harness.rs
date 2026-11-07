//! What Tcode needs to know about a harness's CLI outside a session: how it is
//! found and installed, where it keeps an account, how it signs in, and which
//! native settings Tcode presents for it. Hosts describe each harness to their
//! clients from these facts, so no client decides anything by provider kind.

use serde::{Deserialize, Serialize};

use crate::ProviderKind;

/// A harness CLI's facts, as its adapter knows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessFacts {
    /// The command looked up on `PATH` when no binary path is configured.
    pub binary_name: &'static str,
    /// The documented way to install the CLI on this platform.
    pub install_command: Option<&'static str>,
    /// The CLI's own health check, run in a terminal.
    pub doctor_command: Option<&'static str>,
    /// What the user runs in a terminal to sign the CLI in, for a harness
    /// whose sign-in Tcode does not run.
    pub sign_in_command: Option<&'static str>,
    /// The variable that moves the CLI's config directory, which is what
    /// keeps one account's sign-in apart from another's.
    pub config_dir_env: Option<&'static str>,
    /// Where the CLI keeps its configuration when that variable is unset.
    pub default_config_dir: Option<&'static str>,
    /// The arguments that sign an account in and out, for a harness whose
    /// sign-in Tcode runs ([`crate::Caps::account_login`]).
    pub login_args: &'static [&'static str],
    pub logout_args: &'static [&'static str],
    /// The CLI's own variables for reaching a model API.
    pub endpoint: Option<Endpoint>,
    /// Native on/off switches Tcode presents for the harness, by id.
    pub toggles: &'static [&'static str],
    /// The file its user-level MCP servers are configured in, where that
    /// file is a documented format Tcode has checked against the CLI.
    pub native_mcp: Option<NativeMcpConfig>,
}

/// The format of a harness's MCP configuration (`native_mcp::servers`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeMcpConfig {
    /// A JSON file with an `mcpServers` map.
    McpServersJson,
    /// `[mcp_servers.*]` in a TOML config.
    CodexToml,
    /// `mcp.servers` in what `opencode debug config` resolves.
    OpenCodeResolved,
}

/// An MCP server a harness is configured with on its own, outside Tcode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeMcpServer {
    pub name: String,
    /// Reached over the network rather than started as a local command.
    pub remote: bool,
    /// The command it runs, or the origin it is reached at.
    pub target: String,
    pub enabled: bool,
}

/// The variables a CLI reads to reach a model API other than its vendor's
/// sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url_env: &'static str,
    /// The vendor's own API, which a base URL equal to it does not leave.
    pub native_base_url: &'static str,
    /// Credentials the CLI reads; a third-party profile stores the first.
    pub key_envs: &'static [&'static str],
    pub model_env: Option<&'static str>,
    /// Switches that route the CLI to a cloud provider instead.
    pub backend_switches: &'static [&'static str],
    /// Whether this environment alone describes a complete endpoint, so Tcode
    /// offers named endpoint profiles for it.
    pub profiles: bool,
    pub presets: &'static [EndpointPreset],
}

/// A known endpoint a profile can start from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointPreset {
    pub name: &'static str,
    pub base_url: &'static str,
    pub model: Option<&'static str>,
}

/// pi's switch that loads the project's own `.pi` extensions (`--approve`).
pub const TRUST_PROJECT_EXTENSIONS: &str = "trust_project_extensions";

const CLAUDE_ENDPOINT: Endpoint = Endpoint {
    base_url_env: "ANTHROPIC_BASE_URL",
    native_base_url: "https://api.anthropic.com",
    key_envs: &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
    model_env: Some("ANTHROPIC_MODEL"),
    backend_switches: &[
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ],
    profiles: true,
    presets: &[EndpointPreset {
        name: "Kimi",
        base_url: "https://api.kimi.com/coding/",
        model: Some("k3[1m]"),
    }],
};

// Codex reaches a third-party API through a `[model_providers]` table in its
// config.toml rather than through these variables alone, so it offers no
// endpoint profiles; the variables still tell an API-key setup from a
// signed-in account.
const CODEX_ENDPOINT: Endpoint = Endpoint {
    base_url_env: "OPENAI_BASE_URL",
    native_base_url: "https://api.openai.com/v1",
    key_envs: &["OPENAI_API_KEY", "CODEX_API_KEY"],
    model_env: None,
    backend_switches: &[],
    profiles: false,
    presets: &[],
};

impl ProviderKind {
    /// This harness's CLI facts. ACP agents bring their own launch recipe and
    /// negotiate everything else over the session protocol.
    pub fn harness(self) -> HarnessFacts {
        let windows = cfg!(windows);
        let none = HarnessFacts {
            binary_name: "",
            install_command: None,
            doctor_command: None,
            sign_in_command: None,
            config_dir_env: None,
            default_config_dir: None,
            login_args: &[],
            logout_args: &[],
            endpoint: None,
            toggles: &[],
            native_mcp: None,
        };
        match self {
            ProviderKind::ClaudeCode => HarnessFacts {
                binary_name: "claude",
                install_command: Some(if windows {
                    "irm https://claude.ai/install.ps1 | iex"
                } else {
                    "curl -fsSL https://claude.ai/install.sh | bash"
                }),
                doctor_command: Some("claude doctor"),
                sign_in_command: Some("claude auth login"),
                config_dir_env: Some("CLAUDE_CONFIG_DIR"),
                default_config_dir: Some("~/.claude"),
                login_args: &["auth", "login"],
                logout_args: &["auth", "logout"],
                endpoint: Some(CLAUDE_ENDPOINT),
                toggles: &[],
                native_mcp: Some(NativeMcpConfig::McpServersJson),
            },
            ProviderKind::Codex => HarnessFacts {
                binary_name: "codex",
                install_command: Some("npm install -g @openai/codex"),
                doctor_command: Some("codex doctor"),
                sign_in_command: Some("codex login"),
                config_dir_env: Some("CODEX_HOME"),
                default_config_dir: Some("~/.codex"),
                // The device-code flow prints a page and a one-time code
                // instead of opening a local callback server.
                login_args: &["login", "--device-auth"],
                logout_args: &["logout"],
                endpoint: Some(CODEX_ENDPOINT),
                toggles: &[],
                native_mcp: Some(NativeMcpConfig::CodexToml),
            },
            ProviderKind::Pi => HarnessFacts {
                binary_name: "pi",
                install_command: Some("npm install -g @earendil-works/pi-coding-agent"),
                config_dir_env: Some("PI_CODING_AGENT_DIR"),
                default_config_dir: Some("~/.pi/agent"),
                toggles: &[TRUST_PROJECT_EXTENSIONS],
                native_mcp: Some(NativeMcpConfig::McpServersJson),
                ..none
            },
            ProviderKind::OpenCode => HarnessFacts {
                binary_name: "opencode",
                install_command: Some(if windows {
                    "npm install -g opencode-ai"
                } else {
                    "curl -fsSL https://opencode.ai/install | bash"
                }),
                sign_in_command: Some("opencode auth login"),
                native_mcp: Some(NativeMcpConfig::OpenCodeResolved),
                ..none
            },
            ProviderKind::Cursor => HarnessFacts {
                binary_name: "cursor-agent",
                install_command: Some(if windows {
                    "irm 'https://cursor.com/install?win32=true' | iex"
                } else {
                    "curl https://cursor.com/install -fsS | bash"
                }),
                sign_in_command: Some("cursor-agent login"),
                config_dir_env: Some("CURSOR_CONFIG_DIR"),
                default_config_dir: Some("~/.cursor"),
                ..none
            },
            ProviderKind::Grok => HarnessFacts {
                binary_name: "grok",
                install_command: Some("npm install -g @xai-official/grok"),
                sign_in_command: Some("grok login"),
                config_dir_env: Some("GROK_HOME"),
                default_config_dir: Some("~/.grok"),
                ..none
            },
            ProviderKind::Acp => none,
        }
    }
}

/// The config directory a child launched with `launch_env` reads, for a
/// harness that has one.
#[cfg(feature = "process")]
pub fn config_dir(kind: ProviderKind, launch_env: &crate::LaunchEnv) -> Option<std::path::PathBuf> {
    match kind {
        ProviderKind::ClaudeCode => crate::claude::config_dir(launch_env),
        ProviderKind::Codex => crate::codex::home_dir(launch_env),
        ProviderKind::Pi => crate::pi::agent_dir(launch_env),
        _ => None,
    }
}

/// Seed a new account's empty config directory `home` so the CLI starts in it
/// without first-run prompts, copying the user's own configuration from
/// `inherit` when given. `api_key` seeds it for an endpoint profile, which
/// never signs in. Blocking.
#[cfg(feature = "process")]
pub fn prepare_account_home(
    kind: ProviderKind,
    home: &std::path::Path,
    inherit: Option<&std::path::Path>,
    api_key: bool,
) -> std::io::Result<()> {
    match kind {
        ProviderKind::ClaudeCode => {
            // Seeding onboarding starts the CLI straight into API-key mode.
            let seed = if api_key {
                r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#
            } else {
                r#"{"hasCompletedOnboarding":true}"#
            };
            std::fs::write(home.join(".claude.json"), seed)?;
            inherit.map_or(Ok(()), |from| crate::claude::inherit_config(from, home))
        }
        ProviderKind::Codex => {
            inherit.map_or(Ok(()), |from| crate::codex::inherit_config(from, home))
        }
        _ => Ok(()),
    }
}
