//! Collaborations: a decision model and the collaborator models it starts as subagents, each
//! under a mandate the user wrote. A thread that runs under a collaboration is told it
//! through its provider's instruction channel ([`brief`]).

use agent::{EFFORT_OPTION_ID, ProviderKind};
use serde::{Deserialize, Serialize};

use crate::settings::{Settings, provider_key};

/// One model on one account.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: ProviderKind,
    /// `None` on the provider's built-in profile, as on a thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub model: String,
}

impl ModelRef {
    /// The profile the model runs on, a built-in one by its id.
    pub fn profile(&self) -> &str {
        self.profile_id
            .as_deref()
            .unwrap_or_else(|| Settings::builtin_profile_id(self.provider))
    }

    /// Whether a thread set up as `provider`, `profile_id` and `model` runs this model.
    pub fn is(&self, provider: ProviderKind, profile_id: Option<&str>, model: &str) -> bool {
        self.provider == provider
            && self.profile()
                == profile_id.unwrap_or_else(|| Settings::builtin_profile_id(provider))
            && self.model == model
    }
}

/// A collaborator: a model the decision model starts as a subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationMember {
    pub id: String,
    #[serde(flatten)]
    pub model: ModelRef,
    /// The work it is given in this collaboration.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mandate: String,
    /// The reasoning efforts it may be started at, of those its model takes: one fixes
    /// the effort, several leave the choice among them to the decision model, and none
    /// leaves the effort to the model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
}

impl CollaborationMember {
    /// The effort a start naming `asked` runs at: `asked` where the collaboration
    /// allows it, the one effort it allows when none was asked for, and nothing where
    /// it leaves the effort to the model. `Err` holds the efforts to choose from.
    pub fn effort<'a>(&'a self, asked: Option<&'a str>) -> Result<Option<&'a str>, &'a [String]> {
        match (self.efforts.as_slice(), asked) {
            ([], asked) => Ok(asked),
            (allowed, Some(asked)) if allowed.iter().any(|effort| effort == asked) => {
                Ok(Some(asked))
            }
            ([only], None) => Ok(Some(only)),
            (allowed, _) => Err(allowed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collaboration {
    pub id: String,
    pub name: String,
    /// The decision model: the one a thread under the collaboration runs on.
    pub lead: ModelRef,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lead_mandate: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<CollaborationMember>,
    /// The model picker lists it among the favorites too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub favorite: bool,
}

/// How a model is best used, whatever collaboration it works in: what it is good and bad
/// at, how to brief it, which options to start it with. One per model; a decision model is
/// given the guide of each of its collaborators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelGuide {
    #[serde(flatten)]
    pub model: ModelRef,
    /// Empty while the thread that was asked for it has not written it yet.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// The thread that researched it, or was asked to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// When that thread wrote it (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub researched_at: Option<u64>,
}

/// The mandates a collaboration is written from: the decision model's, which a new
/// collaboration starts with, and those a collaborator can be given. They ship with the
/// app and can be fetched anew from the repository; a newer file by `updated_at` wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Templates {
    pub version: u64,
    /// An ISO date, compared as text.
    pub updated_at: String,
    pub lead: String,
    pub roles: Vec<RoleTemplate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleTemplate {
    pub name: String,
    pub mandate: String,
}

const BUNDLED_TEMPLATES: &str = include_str!("../../../assets/collaborations/templates.json");

/// Where the newest templates are published.
pub const TEMPLATES_URL: &str =
    "https://raw.githubusercontent.com/Tryanks/tcode/main/assets/collaborations/templates.json";

impl Templates {
    /// The only file version this build reads.
    const VERSION: u64 = 1;

    pub fn bundled() -> Self {
        Self::parse(BUNDLED_TEMPLATES).expect("the bundled collaboration templates are valid")
    }

    /// A templates file, refused when it is of another version or names no mandate.
    pub fn parse(text: &str) -> Result<Self, String> {
        let templates: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if templates.version != Self::VERSION {
            return Err(format!(
                "templates version {} is not one this build reads",
                templates.version
            ));
        }
        if templates.lead.trim().is_empty() {
            return Err("the templates name no mandate for the decision model".into());
        }
        Ok(templates)
    }
}

const BRIEF: &str = include_str!("../../../assets/collaborations/brief.md");

/// What a thread under `collaboration` is told, through its provider's instruction
/// channel. `names` gives a model's display name and its account's, `guide` what is known
/// of how the model is best used, and `default_mandate` is the decision model's where the
/// user wrote none.
pub fn brief(
    collaboration: &Collaboration,
    default_mandate: &str,
    names: impl Fn(&ModelRef) -> (String, String),
    guide: impl Fn(&ModelRef) -> Option<String>,
) -> String {
    let members = if collaboration.members.is_empty() {
        "The collaboration names no collaborator yet.".to_owned()
    } else {
        collaboration
            .members
            .iter()
            .map(|member| {
                let (model, account) = names(&member.model);
                let profile = match &member.model.profile_id {
                    Some(profile) => format!(", profile \"{profile}\""),
                    None => String::new(),
                };
                let mandate = if member.mandate.trim().is_empty() {
                    "The user wrote no mandate for it."
                } else {
                    member.mandate.trim()
                };
                let guide = guide(&member.model)
                    .map(|guide| guide.trim().to_owned())
                    .filter(|guide| !guide.is_empty())
                    .map(|guide| format!("\n\nHow to use this model:\n\n{guide}"))
                    .unwrap_or_default();
                let quoted = |efforts: &[String]| {
                    efforts
                        .iter()
                        .map(|effort| format!("\"{effort}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let effort = match member.efforts.as_slice() {
                    [] => String::new(),
                    [only] => format!(", options {{\"{EFFORT_OPTION_ID}\": \"{only}\"}}"),
                    several => format!(
                        ", and options {{\"{EFFORT_OPTION_ID}\": …}} set to one of {}, as much as the \
                         piece of work calls for",
                        quoted(several)
                    ),
                };
                format!(
                    "### {model}\n\n{model} on {account}. Start with provider \"{}\"{profile}, model \"{}\"{effort}.\n\nMandate: {mandate}{guide}",
                    provider_key(member.model.provider),
                    member.model.model,
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let lead_mandate = if collaboration.lead_mandate.trim().is_empty() {
        default_mandate.trim()
    } else {
        collaboration.lead_mandate.trim()
    };
    BRIEF
        .replacen("{name}", &collaboration.name, 1)
        .replacen("{lead_mandate}", lead_mandate, 1)
        .replacen("{members}", &members, 1)
}

/// What a thread is asked so that it researches `model` and writes its guide: a message
/// the user reads, may change, and sends. `model_name` and `account` are the model's and
/// its account's names as the user sees them.
pub fn research_prompt(model: &ModelRef, model_name: &str, account: &str) -> String {
    format!(
        "Research how the model {model_name} (id `{model_id}`, run through {harness} on the account {account}) is best used, and write its guide for Tcode.\n\n\
         The guide is read by another model, which hands this one work as a subagent and has to know how to get the best out of it.\n\n\
         1. Find what the model's maker publishes about it: its model card, its release notes, its prompting or usage guide, its pricing. Use primary sources (the maker's own site and documentation), search for them, and read them. If the maker published nothing about this exact model, say so and write only what the sources you did find support.\n\
         2. Write the guide in English, as a few short paragraphs or a short list, covering: what kinds of work to give the model and what to keep from it; how to brief it; which reasoning effort or other option to start it with, where the maker gives guidance; and its speed and cost next to its maker's other models, where the sources say. End it with a \"Sources:\" list of the pages it rests on. State only what the sources support; no marketing language.\n\
         3. Store it with the tcode_control tool `set_model_guide`, which this thread was given for this model.\n\
         4. Reply with the guide you stored.",
        model_id = model.model,
        harness = model.provider.display_name(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The collaborations a settings file holds are read as written.
    #[test]
    fn a_stored_collaboration_reads_back() {
        let stored = serde_json::json!([{
            "id": "delivery",
            "name": "Delivery",
            "lead": {"provider": "claude_code", "model": "claude-fable-5-2"},
            "lead_mandate": "Understand the task.",
            "members": [{
                "id": "m1",
                "provider": "codex",
                "profile_id": "second",
                "model": "gpt-6.1-sol",
                "mandate": "Implements."
            }]
        }]);
        let collaborations: Vec<Collaboration> = serde_json::from_value(stored.clone()).unwrap();
        assert_eq!(serde_json::to_value(&collaborations).unwrap(), stored);
    }

    /// The templates that ship with the app are a file this build reads.
    #[test]
    fn the_bundled_templates_are_read() {
        let templates = Templates::bundled();
        assert!(!templates.roles.is_empty());
        assert!(
            Templates::parse(r#"{"version":2,"updatedAt":"2027-01-01","lead":"x","roles":[]}"#)
                .is_err()
        );
    }
}
