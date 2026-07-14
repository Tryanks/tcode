//! Persisted Agent Client Protocol configuration.

use agent::AcpLaunch;
use serde::{Deserialize, Serialize};

/// An installed agent, as persisted in settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledAcpAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub icon: Option<String>,
    /// The resolved recipe: exactly what we will spawn.
    pub launch: AcpLaunch,
    /// SHA-256 of the downloaded archive, for binary distributions.
    ///
    /// The registry publishes no digests (there is no `sha256` field in the
    /// schema — zed does not verify one either), so this is what we computed at
    /// install time rather than a checked-against-upstream signature. It lets us
    /// tell whether a re-download changed, which is the most integrity the index
    /// currently affords.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra environment for this agent's process (Settings → Providers).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Extra CLI arguments appended at launch.
    #[serde(default)]
    pub launch_args: Option<String>,
}

fn default_true() -> bool {
    true
}

impl InstalledAcpAgent {
    /// The whitespace-split launch arguments (mirrors Claude's "Launch arguments").
    pub fn extra_args(&self) -> Vec<String> {
        self.launch_args
            .as_deref()
            .map(|args| args.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}
