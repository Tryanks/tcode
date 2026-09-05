use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender};

/// One logical client endpoint attached to a [`HostMux`].
pub struct Connection {
    pub to_host: Sender<String>,
    pub from_host: Receiver<String>,
}

#[derive(Clone)]
pub struct HostMux {
    inner: Arc<Inner>,
}

struct Inner {
    ingress: Sender<Ingress>,
    next_connection: AtomicU64,
}

enum Ingress {
    Add(u64, Sender<String>),
    Line(u64, String),
    Closed(u64),
}

impl HostMux {
    pub fn new(to_host: Sender<String>, from_host: Receiver<String>) -> Self {
        let (ingress, ingress_rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("tcode-remote-mux".into())
            .spawn(move || smol::block_on(pump(to_host, from_host, ingress_rx)))
            .expect("failed to spawn remote mux thread");
        Self {
            inner: Arc::new(Inner {
                ingress,
                next_connection: AtomicU64::new(1),
            }),
        }
    }

    pub fn attach(&self) -> Connection {
        let connection_id = self.inner.next_connection.fetch_add(1, Ordering::Relaxed);
        let (client_tx, client_rx) = async_channel::unbounded();
        let (output_tx, output_rx) = async_channel::unbounded();
        let ingress = self.inner.ingress.clone();
        let _ = ingress.try_send(Ingress::Add(connection_id, output_tx));
        std::thread::Builder::new()
            .name(format!("tcode-remote-mux-{connection_id}"))
            .spawn(move || {
                while let Ok(line) = client_rx.recv_blocking() {
                    if ingress
                        .send_blocking(Ingress::Line(connection_id, line))
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = ingress.send_blocking(Ingress::Closed(connection_id));
            })
            .expect("failed to spawn remote mux connection thread");
        Connection {
            to_host: client_tx,
            from_host: output_rx,
        }
    }
}

async fn pump(to_host: Sender<String>, from_host: Receiver<String>, ingress: Receiver<Ingress>) {
    let mut clients = HashMap::<u64, Sender<String>>::new();
    let mut subscriptions = HashMap::<u64, HashSet<String>>::new();
    let routes = Arc::new(Mutex::new(HashMap::<u64, (u64, u64)>::new()));
    let mut next_global_id = 1_u64;

    loop {
        enum Input {
            Client(Result<Ingress, async_channel::RecvError>),
            Host(Result<String, async_channel::RecvError>),
        }
        let input =
            futures_lite::future::race(async { Input::Client(ingress.recv().await) }, async {
                Input::Host(from_host.recv().await)
            })
            .await;
        match input {
            Input::Client(Ok(Ingress::Add(id, sender))) => {
                clients.insert(id, sender);
                subscriptions.insert(id, HashSet::new());
            }
            Input::Client(Ok(Ingress::Closed(id))) => {
                clients.remove(&id);
                if let Some(topics) = subscriptions.remove(&id) {
                    for topic in topics {
                        if !subscriptions.values().any(|topics| topics.contains(&topic)) {
                            let line = format!(
                                "{{\"id\":0,\"payload\":{{\"type\":\"unsubscribe\",\"content\":{{\"topic\":{topic}}}}}}}\n"
                            );
                            if to_host.send(line).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                routes.lock().unwrap().retain(|_, route| route.0 != id);
            }
            Input::Client(Ok(Ingress::Line(connection_id, line))) => {
                if let Some((kind, topic)) = subscription_change(&line) {
                    let topics = subscriptions.entry(connection_id).or_default();
                    if kind == "subscribe" {
                        topics.insert(topic.clone());
                    } else {
                        topics.remove(&topic);
                        if subscriptions.values().any(|topics| topics.contains(&topic)) {
                            if let Some((_, local_id)) = rewrite_client_id(&line, 0) {
                                let ack = tcode_protocol::HostMessage::Ack {
                                    id: local_id,
                                    result: Ok(tcode_protocol::CommandResponse::Unit),
                                };
                                if let Some(sender) = clients.get(&connection_id) {
                                    let _ =
                                        sender.try_send(tcode_protocol::encode_line(&ack).unwrap());
                                }
                            }
                            continue;
                        }
                    }
                }
                let Some((rewritten, local_id)) = rewrite_client_id(&line, next_global_id) else {
                    continue;
                };
                routes
                    .lock()
                    .unwrap()
                    .insert(next_global_id, (connection_id, local_id));
                next_global_id = next_global_id.wrapping_add(1).max(1);
                if to_host.send(rewritten).await.is_err() {
                    break;
                }
            }
            Input::Client(Err(_)) | Input::Host(Err(_)) => break,
            Input::Host(Ok(line)) => {
                let Some(value) = serde_json::from_str::<serde_json::Value>(line.trim_end()).ok()
                else {
                    continue;
                };
                match value.get("type").and_then(serde_json::Value::as_str) {
                    Some("event") => {
                        let Some(content) = value.get("content") else {
                            continue;
                        };
                        let Some(topic) = content
                            .get("topic")
                            .and_then(|topic| serde_json::to_string(topic).ok())
                        else {
                            continue;
                        };
                        if let Some(request_id) = content
                            .get("request_id")
                            .and_then(serde_json::Value::as_u64)
                        {
                            if let Some((connection_id, local_id)) =
                                routes.lock().unwrap().get(&request_id).copied()
                                && subscriptions
                                    .get(&connection_id)
                                    .is_some_and(|topics| topics.contains(&topic))
                                && let Some(sender) = clients.get(&connection_id)
                                && let Some(line) =
                                    replace_field(&line, &["content", "request_id"], local_id)
                            {
                                let _ = sender.try_send(line);
                            }
                        } else {
                            for (id, sender) in &clients {
                                if subscriptions
                                    .get(id)
                                    .is_some_and(|topics| topics.contains(&topic))
                                {
                                    let _ = sender.try_send(line.clone());
                                }
                            }
                        }
                    }
                    Some("ack" | "query_result") => {
                        let Some(global_id) = value
                            .get("content")
                            .and_then(|content| content.get("id"))
                            .and_then(serde_json::Value::as_u64)
                        else {
                            continue;
                        };
                        let Some((connection_id, local_id)) =
                            routes.lock().unwrap().remove(&global_id)
                        else {
                            continue;
                        };
                        let Some(rewritten) = rewrite_host_id(&line, local_id) else {
                            continue;
                        };
                        if clients
                            .get(&connection_id)
                            .is_some_and(|sender| sender.try_send(rewritten).is_err())
                        {
                            clients.remove(&connection_id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    clients.clear();
}

fn subscription_change(line: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = value.get("payload")?;
    let kind = payload.get("type")?.as_str()?;
    if !matches!(kind, "subscribe" | "unsubscribe") {
        return None;
    }
    Some((
        kind.to_string(),
        serde_json::to_string(payload.get("content")?.get("topic")?).ok()?,
    ))
}

fn rewrite_client_id(line: &str, id: u64) -> Option<(String, u64)> {
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    let local_id = value.get("id")?.as_u64()?;
    Some((replace_field(line, &["id"], id)?, local_id))
}

fn rewrite_host_id(line: &str, id: u64) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    value.get("content")?.get("id")?.as_u64()?;
    replace_field(line, &["content", "id"], id)
}

/// Locate a field through JSON objects and replace only its value bytes. The
/// full serde parse above validates the input; this small scanner preserves all
/// whitespace, ordering, escaping, and the trailing NDJSON newline.
fn replace_field(line: &str, path: &[&str], id: u64) -> Option<String> {
    let bytes = line.as_bytes();
    let mut object_start = skip_space(bytes, 0);
    let mut span = None;
    for (index, field) in path.iter().enumerate() {
        span = object_field_span(bytes, object_start, field);
        let (start, _) = span?;
        if index + 1 < path.len() {
            object_start = skip_space(bytes, start);
            if bytes.get(object_start) != Some(&b'{') {
                return None;
            }
        }
    }
    let (start, end) = span?;
    let mut result = String::with_capacity(line.len() + 20);
    result.push_str(&line[..start]);
    result.push_str(&id.to_string());
    result.push_str(&line[end..]);
    Some(result)
}

fn object_field_span(bytes: &[u8], object_start: usize, wanted: &str) -> Option<(usize, usize)> {
    if bytes.get(object_start) != Some(&b'{') {
        return None;
    }
    let mut cursor = object_start + 1;
    loop {
        cursor = skip_space(bytes, cursor);
        match bytes.get(cursor)? {
            b'}' => return None,
            b',' => {
                cursor += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let key_end = string_end(bytes, cursor)?;
        let key: String = serde_json::from_slice(&bytes[cursor..key_end]).ok()?;
        cursor = skip_space(bytes, key_end);
        if bytes.get(cursor) != Some(&b':') {
            return None;
        }
        let value_start = skip_space(bytes, cursor + 1);
        let value_end = value_end(bytes, value_start)?;
        if key == wanted {
            return Some((value_start, value_end));
        }
        cursor = value_end;
    }
}

fn skip_space(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    cursor
}

fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start + 1;
    let mut escaped = false;
    while let Some(byte) = bytes.get(cursor) {
        cursor += 1;
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some(cursor);
        }
    }
    None
}

fn value_end(bytes: &[u8], start: usize) -> Option<usize> {
    match *bytes.get(start)? {
        b'"' => string_end(bytes, start),
        b'{' | b'[' => {
            let opening = bytes[start];
            let closing = if opening == b'{' { b'}' } else { b']' };
            let mut depth = 0_u32;
            let mut cursor = start;
            while let Some(byte) = bytes.get(cursor) {
                if *byte == b'"' {
                    cursor = string_end(bytes, cursor)?;
                    continue;
                }
                if *byte == opening {
                    depth += 1;
                } else if *byte == closing {
                    depth -= 1;
                    if depth == 0 {
                        return Some(cursor + 1);
                    }
                }
                cursor += 1;
            }
            None
        }
        _ => {
            let mut cursor = start;
            while bytes.get(cursor).is_some_and(|byte| {
                !matches!(byte, b',' | b'}' | b']') && !byte.is_ascii_whitespace()
            }) {
                cursor += 1;
            }
            (cursor > start).then_some(cursor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_route_to_owner() {
        let (to_host, host_rx) = async_channel::unbounded();
        let (host_tx, from_host) = async_channel::unbounded();
        let mux = HostMux::new(to_host, from_host);
        let one = mux.attach();
        let two = mux.attach();
        // Each connection forwards on its own thread, so the two lines can
        // reach the mux in either order. Tag them by topic and resolve
        // ownership from the payload rather than from arrival order.
        one.to_host
            .send_blocking("{\"id\":7,\"payload\":{\"type\":\"subscribe\",\"content\":{\"topic\":{\"type\":\"index\"}}}}\n".into())
            .unwrap();
        two.to_host
            .send_blocking("{\"id\":7,\"payload\":{\"type\":\"subscribe\",\"content\":{\"topic\":{\"type\":\"providers\"}}}}\n".into())
            .unwrap();
        let mut ids = HashMap::<String, u64>::new();
        for _ in 0..2 {
            let line: serde_json::Value =
                serde_json::from_str(host_rx.recv_blocking().unwrap().trim_end()).unwrap();
            let topic = line["payload"]["content"]["topic"]["type"]
                .as_str()
                .unwrap()
                .to_owned();
            ids.insert(topic, line["id"].as_u64().unwrap());
        }
        let first_id = ids["index"];
        let second_id = ids["providers"];
        assert_ne!(first_id, second_id);
        host_tx
            .send_blocking(format!(
                "{{\"type\":\"ack\",\"content\":{{\"id\":{second_id},\"result\":{{\"Ok\":\"unit\"}}}}}}\n"
            ))
            .unwrap();
        let reply: serde_json::Value =
            serde_json::from_str(two.from_host.recv_blocking().unwrap().trim_end()).unwrap();
        assert_eq!(reply["content"]["id"], 7);
        assert!(one.from_host.try_recv().is_err());
    }

    #[test]
    fn only_subscribers_receive_events_and_last_unsubscribe_releases_topic() {
        let (to_host, host_rx) = async_channel::unbounded();
        let (host_tx, from_host) = async_channel::unbounded();
        let mux = HostMux::new(to_host, from_host);
        let one = mux.attach();
        let two = mux.attach();
        let send = |connection: &Connection, kind: &str, id: u64| {
            connection.to_host.send_blocking(format!(r#"{{"id":{id},"payload":{{"type":"{kind}","content":{{"topic":{{"type":"session_events","content":{{"session_id":"one"}}}}}}}}}}"#)).unwrap();
        };
        send(&one, "subscribe", 1);
        let _: String = host_rx.recv_blocking().unwrap();
        let event = r#"{"type":"event","content":{"topic":{"type":"session_events","content":{"session_id":"one"}},"event":{"type":"session_snapshot","content":{"from":0,"records":[]}}}}"#;
        host_tx.send_blocking(event.into()).unwrap();
        assert_eq!(one.from_host.recv_blocking().unwrap(), event);
        assert!(two.from_host.try_recv().is_err());
        send(&two, "subscribe", 2);
        let _: String = host_rx.recv_blocking().unwrap();
        send(&one, "unsubscribe", 3);
        let ack = one.from_host.recv_blocking().unwrap();
        assert!(ack.contains("ack"));
        assert!(
            host_rx.try_recv().is_err(),
            "another client still owns this subscription"
        );
        host_tx.send_blocking(event.into()).unwrap();
        assert_eq!(two.from_host.recv_blocking().unwrap(), event);
        assert!(one.from_host.try_recv().is_err());
        drop(two.to_host);
        let unsubscribe: serde_json::Value =
            serde_json::from_str(&host_rx.recv_blocking().unwrap()).unwrap();
        assert_eq!(unsubscribe["payload"]["type"], "unsubscribe");
    }

    #[test]
    fn rewrite_preserves_every_other_byte() {
        let input = " { \"payload\" : {\"x\": [1, 2]}, \"id\" : 42 }\n";
        let (rewritten, local) = rewrite_client_id(input, 987).unwrap();
        assert_eq!(local, 42);
        assert_eq!(
            rewritten,
            " { \"payload\" : {\"x\": [1, 2]}, \"id\" : 987 }\n"
        );
        let output = "{\"type\": \"ack\", \"content\": { \"result\": {}, \"id\": 987 }}\n";
        assert_eq!(
            rewrite_host_id(output, 42).unwrap(),
            "{\"type\": \"ack\", \"content\": { \"result\": {}, \"id\": 42 }}\n"
        );
    }
}
