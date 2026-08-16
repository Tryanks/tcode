use std::collections::HashMap;
use std::hash::Hash;

use crate::AgentEvent;

pub(crate) type PendingRequests<K, V> = HashMap<K, V>;

pub(crate) fn drain_resolved<K, V>(pending: &mut PendingRequests<K, V>) -> Vec<(K, V, AgentEvent)>
where
    K: Eq + Hash + Clone + Into<String>,
{
    pending
        .drain()
        .map(|(request_id, value)| {
            let event = AgentEvent::UserInputResolved {
                request_id: request_id.clone().into(),
                answers: serde_json::Map::new(),
            };
            (request_id, value, event)
        })
        .collect()
}
