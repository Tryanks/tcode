//! Bounded immutable observations and independently bounded output pages.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use crate::backend::RootInfo;
use crate::outline::{
    MAX_MODEL_LINES, PAGE_BYTES, PREVIEW_BYTES, UiNode, assign_refs, assign_refs_from_previous,
    output_exceeds_limit, safe_prefix,
};

pub const OBSERVATION_CAPACITY: usize = 8;
pub const OUTPUT_CAPACITY: usize = 32;

#[derive(Debug)]
pub struct Observation {
    pub state_id: String,
    pub root: RootInfo,
    pub root_epoch: u64,
    pub tree: UiNode,
    pub screenshot_png: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    Evicted(String),
    Stale {
        state_id: String,
        state_epoch: u64,
        current_epoch: u64,
    },
    UnknownElement(String),
    UnknownOutput(String),
    OutputOwnerMismatch {
        output_ref: String,
        expected: String,
        actual: Option<String>,
    },
    InvalidOffset(usize),
}

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Evicted(state_id) => write!(
                formatter,
                "state {state_id} was evicted from the observation cache; call observe_ui again"
            ),
            Self::Stale {
                state_id,
                state_epoch,
                current_epoch,
            } => write!(
                formatter,
                "state {state_id} is stale (root epoch {state_epoch}, current {current_epoch}); call observe_ui again"
            ),
            Self::UnknownElement(ref_id) => write!(
                formatter,
                "element ref {ref_id} is not owned by this state; call observe_ui again if the UI changed"
            ),
            Self::UnknownOutput(output_ref) => write!(
                formatter,
                "output continuation {output_ref} was evicted or does not exist; rerun the originating tool"
            ),
            Self::OutputOwnerMismatch {
                output_ref,
                expected,
                actual,
            } => write!(
                formatter,
                "output continuation {output_ref} belongs to state {}, not {expected}",
                actual.as_deref().unwrap_or("<none>")
            ),
            Self::InvalidOffset(offset) => write!(
                formatter,
                "byte offset {offset} is outside the text or splits a UTF-8 character"
            ),
        }
    }
}

impl std::error::Error for StateError {}

#[derive(Debug, Clone)]
struct OutputEntry {
    owner_state: Option<String>,
    text: Arc<str>,
    initial_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPage {
    pub output_ref: String,
    pub owner_state: Option<String>,
    pub text: String,
    pub offset: usize,
    pub next_offset: usize,
    pub total_bytes: usize,
    pub eof: bool,
}

#[derive(Debug)]
pub struct StateStore {
    observation_capacity: usize,
    output_capacity: usize,
    next_state: u64,
    next_output: u64,
    observations: HashMap<String, Arc<Observation>>,
    observation_lru: VecDeque<String>,
    root_epochs: HashMap<String, u64>,
    outputs: HashMap<String, OutputEntry>,
    output_lru: VecDeque<String>,
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new(OBSERVATION_CAPACITY, OUTPUT_CAPACITY)
    }
}

impl StateStore {
    pub fn new(observation_capacity: usize, output_capacity: usize) -> Self {
        assert!(observation_capacity > 0);
        assert!(output_capacity > 0);
        Self {
            observation_capacity,
            output_capacity,
            next_state: 1,
            next_output: 1,
            observations: HashMap::new(),
            observation_lru: VecDeque::new(),
            root_epochs: HashMap::new(),
            outputs: HashMap::new(),
            output_lru: VecDeque::new(),
        }
    }

    pub fn insert_observation(
        &mut self,
        root: RootInfo,
        mut tree: UiNode,
        screenshot_png: Option<Vec<u8>>,
    ) -> Arc<Observation> {
        let identity = root.identity();
        let previous = self
            .observation_lru
            .iter()
            .rev()
            .filter_map(|state_id| self.observations.get(state_id))
            .find(|observation| observation.root.identity() == identity)
            .cloned();
        if let Some(previous) = previous {
            assign_refs_from_previous(&previous.tree, &mut tree);
        } else {
            assign_refs(&mut tree);
        }

        let epoch = self.root_epochs.entry(identity).or_default();
        *epoch += 1;
        let state_id = format!("S{}", self.next_state);
        self.next_state += 1;
        let observation = Arc::new(Observation {
            state_id: state_id.clone(),
            root,
            root_epoch: *epoch,
            tree,
            screenshot_png,
        });
        self.observations
            .insert(state_id.clone(), Arc::clone(&observation));
        touch(&mut self.observation_lru, &state_id);
        while self.observations.len() > self.observation_capacity {
            if let Some(evicted) = self.observation_lru.pop_front() {
                self.observations.remove(&evicted);
            }
        }
        observation
    }

    pub fn get(&mut self, state_id: &str) -> Result<Arc<Observation>, StateError> {
        let observation = self
            .observations
            .get(state_id)
            .cloned()
            .ok_or_else(|| StateError::Evicted(state_id.to_string()))?;
        touch(&mut self.observation_lru, state_id);
        Ok(observation)
    }

    pub fn validate_for_action(&mut self, state_id: &str) -> Result<Arc<Observation>, StateError> {
        let observation = self.get(state_id)?;
        let current_epoch = self
            .root_epochs
            .get(&observation.root.identity())
            .copied()
            .unwrap_or_default();
        if current_epoch != observation.root_epoch {
            return Err(StateError::Stale {
                state_id: state_id.to_string(),
                state_epoch: observation.root_epoch,
                current_epoch,
            });
        }
        Ok(observation)
    }

    pub fn register_output(
        &mut self,
        owner_state: Option<&str>,
        text: impl Into<Arc<str>>,
        initial_offset: usize,
    ) -> String {
        let output_ref = format!("@o{}", self.next_output);
        self.next_output += 1;
        self.outputs.insert(
            output_ref.clone(),
            OutputEntry {
                owner_state: owner_state.map(str::to_owned),
                text: text.into(),
                initial_offset,
            },
        );
        touch(&mut self.output_lru, &output_ref);
        while self.outputs.len() > self.output_capacity {
            if let Some(evicted) = self.output_lru.pop_front() {
                self.outputs.remove(&evicted);
            }
        }
        output_ref
    }

    pub fn bound_model_text(&mut self, owner_state: Option<&str>, text: String) -> String {
        if !output_exceeds_limit(&text) {
            return text;
        }
        let (preview, offset) = safe_prefix(&text, PREVIEW_BYTES, MAX_MODEL_LINES);
        let preview = preview.to_string();
        let output_ref = self.register_output(owner_state, Arc::<str>::from(text), offset);
        format!(
            "{preview}\n\n[output truncated: use read_text with ref {output_ref} and offset {offset}]"
        )
    }

    pub fn read_output(
        &mut self,
        output_ref: &str,
        state_id: Option<&str>,
        offset: Option<usize>,
    ) -> Result<OutputPage, StateError> {
        let entry = self
            .outputs
            .get(output_ref)
            .cloned()
            .ok_or_else(|| StateError::UnknownOutput(output_ref.to_string()))?;
        if let Some(expected) = state_id
            && entry.owner_state.as_deref() != Some(expected)
        {
            return Err(StateError::OutputOwnerMismatch {
                output_ref: output_ref.to_string(),
                expected: expected.to_string(),
                actual: entry.owner_state,
            });
        }
        touch(&mut self.output_lru, output_ref);
        page(
            output_ref,
            entry.owner_state,
            &entry.text,
            offset.unwrap_or(entry.initial_offset),
        )
    }

    pub fn page_element_text(
        &mut self,
        state_id: &str,
        ref_id: &str,
        offset: usize,
    ) -> Result<OutputPage, StateError> {
        let observation = self.get(state_id)?;
        let node = observation
            .tree
            .find(ref_id)
            .ok_or_else(|| StateError::UnknownElement(ref_id.to_string()))?;
        let text: Arc<str> = Arc::from(node.text());
        if text.len().saturating_sub(offset) > PAGE_BYTES {
            let output_ref = self.register_output(Some(state_id), Arc::clone(&text), offset);
            self.read_output(&output_ref, Some(state_id), Some(offset))
        } else {
            page(ref_id, Some(state_id.to_string()), &text, offset)
        }
    }

    #[cfg(test)]
    fn contains(&self, state_id: &str) -> bool {
        self.observations.contains_key(state_id)
    }
}

fn page(
    output_ref: &str,
    owner_state: Option<String>,
    text: &str,
    offset: usize,
) -> Result<OutputPage, StateError> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return Err(StateError::InvalidOffset(offset));
    }
    let remaining = &text[offset..];
    let (content, consumed) = safe_prefix(remaining, PAGE_BYTES, MAX_MODEL_LINES);
    let next_offset = offset + consumed;
    Ok(OutputPage {
        output_ref: output_ref.to_string(),
        owner_state,
        text: content.to_string(),
        offset,
        next_offset,
        total_bytes: text.len(),
        eof: next_offset == text.len(),
    })
}

fn touch(lru: &mut VecDeque<String>, key: &str) {
    if let Some(index) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(index);
    }
    lru.push_back(key.to_string());
}

static STORE: OnceLock<Mutex<StateStore>> = OnceLock::new();

pub fn global() -> &'static Mutex<StateStore> {
    STORE.get_or_init(|| Mutex::new(StateStore::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::Frame;

    fn root(window_id: u32) -> RootInfo {
        RootInfo {
            app_name: "Test".into(),
            pid: 42,
            title: format!("Window {window_id}"),
            window_id,
            frame: Frame {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
            },
            ..RootInfo::default()
        }
    }

    fn tree(title: &str) -> UiNode {
        UiNode {
            role: "window".into(),
            title: title.into(),
            enabled: true,
            ..UiNode::default()
        }
    }

    #[test]
    fn lru_evicts_least_recently_used_observation() {
        let mut store = StateStore::new(2, 4);
        let first = store.insert_observation(root(1), tree("one"), None);
        let second = store.insert_observation(root(2), tree("two"), None);
        store.get(&first.state_id).unwrap();
        let third = store.insert_observation(root(3), tree("three"), None);
        assert!(store.contains(&first.state_id));
        assert!(!store.contains(&second.state_id));
        assert!(store.contains(&third.state_id));
        assert!(matches!(
            store.get(&second.state_id),
            Err(StateError::Evicted(_))
        ));
    }

    #[test]
    fn newer_root_epoch_rejects_actions_from_old_state() {
        let mut store = StateStore::new(8, 4);
        let first = store.insert_observation(root(1), tree("one"), None);
        let second = store.insert_observation(root(1), tree("one changed"), None);
        assert!(matches!(
            store.validate_for_action(&first.state_id),
            Err(StateError::Stale { .. })
        ));
        assert!(store.validate_for_action(&second.state_id).is_ok());
    }

    #[test]
    fn bounded_output_continuation_round_trips_without_mutation() {
        let mut store = StateStore::new(2, 4);
        let original = "0123456789abcdef\n".repeat(4_000);
        let visible = store.bound_model_text(Some("S9"), original.clone());
        assert!(visible.len() < 20 * 1024);
        let output_ref = visible
            .split_whitespace()
            .find(|part| part.starts_with("@o"))
            .unwrap();
        let mut rebuilt = visible
            .split("\n\n[output truncated")
            .next()
            .unwrap()
            .to_string();
        let mut offset = rebuilt.len();
        loop {
            let page = store
                .read_output(output_ref, Some("S9"), Some(offset))
                .unwrap();
            rebuilt.push_str(&page.text);
            if page.eof {
                break;
            }
            offset = page.next_offset;
        }
        assert_eq!(rebuilt, original);

        let repeated = store
            .read_output(output_ref, Some("S9"), Some(offset))
            .unwrap();
        let repeated_again = store
            .read_output(output_ref, Some("S9"), Some(offset))
            .unwrap();
        assert_eq!(repeated, repeated_again);
    }
}
