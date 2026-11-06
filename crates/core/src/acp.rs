//! Persisted Agent Client Protocol configuration.

use agent::AcpLaunch;
use serde::{Deserialize, Deserializer, Serialize};

use crate::settings::EnvVar;

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
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra environment for this agent's process (Settings → Harnesses).
    /// Sensitive values are stored under
    /// [`Settings::acp_secret_id`](crate::settings::Settings::acp_secret_id).
    #[serde(default, deserialize_with = "env_rows")]
    pub env: Vec<EnvVar>,
    /// Extra CLI arguments appended at launch.
    #[serde(default)]
    pub launch_args: Option<String>,
}

/// Serializable edits the UI can make to an installed ACP agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcpAgentPatch {
    SetEnabled { enabled: bool },
    SetLaunchArgs { launch_args: Option<String> },
    SetEnv { env: Vec<EnvVar> },
}

/// The agent's variables, also as the `[name, value]` pairs earlier builds
/// wrote.
fn env_rows<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<EnvVar>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Row {
        Var(EnvVar),
        Pair(String, String),
    }
    Ok(Vec::<Row>::deserialize(deserializer)?
        .into_iter()
        .map(|row| match row {
            Row::Var(var) => var,
            Row::Pair(name, value) => EnvVar {
                name,
                value,
                sensitive: false,
            },
        })
        .collect())
}

fn default_true() -> bool {
    true
}

impl InstalledAcpAgent {
    /// Whether new sessions may use this agent. An agent that duplicates a
    /// native integration stays installed, so its existing sessions resume and
    /// it can be removed, but new sessions use the native provider.
    pub fn offered_for_new_sessions(&self) -> bool {
        !agent::HIDDEN_ACP_AGENT_IDS.contains(&self.id.as_str())
    }

    /// The whitespace-split launch arguments (mirrors Claude's "Launch arguments").
    pub fn extra_args(&self) -> Vec<String> {
        self.launch_args
            .as_deref()
            .map(|args| args.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An agent saved by a build that kept its variables as pairs loads with
    /// them as plain rows.
    #[test]
    fn an_agent_saved_with_variable_pairs_loads_them_as_rows() {
        let agent: InstalledAcpAgent = serde_json::from_str(
            r#"{"id":"gemini","name":"Gemini CLI","launch":{"kind":"custom","command":"gemini","args":[],"env":[]},
                "env":[["GEMINI_MODEL","gemini-3-pro"],["A","b=c"]]}"#,
        )
        .unwrap();
        assert_eq!(
            agent.env,
            vec![
                EnvVar {
                    name: "GEMINI_MODEL".into(),
                    value: "gemini-3-pro".into(),
                    sensitive: false,
                },
                EnvVar {
                    name: "A".into(),
                    value: "b=c".into(),
                    sensitive: false,
                },
            ]
        );
    }
}
