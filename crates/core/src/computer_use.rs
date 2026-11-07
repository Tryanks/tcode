//! The question computer use asks the user when an app has to come to the
//! front for keys to reach it, and the answers it takes.

use serde::{Deserialize, Serialize};

/// What the keys would do once the app is in front.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForegroundAction {
    /// Type this many characters.
    Type { characters: usize },
    /// Press these keys or chords.
    Keys { keys: Vec<String> },
    /// Select the field's text and type this many characters over it.
    Replace { characters: usize },
}

/// The user's answer to a [`ForegroundPrompt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForegroundAnswer {
    /// Bring it to the front for this step.
    Once,
    /// Bring it to the front for this step and remember the app as allowed.
    Always,
    Deny,
}

/// A thread's computer use waiting for the user to say whether an app may
/// come to the front for typing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForegroundPrompt {
    pub id: String,
    /// The app's name as macOS shows it.
    pub app_name: String,
    pub bundle_id: String,
    /// The title of the window the keys are for; empty when it has none.
    pub window_title: String,
    pub action: ForegroundAction,
    /// Unix seconds.
    pub asked_at: u64,
}
