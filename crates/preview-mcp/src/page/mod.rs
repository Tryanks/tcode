//! The page agent: the script Tcode installs at document start in every
//! preview page, and the messages it and Tcode exchange.
//!
//! The agent keeps refs, builds the accessibility outline, acts on the page
//! with synthetic input, waits on DOM conditions, and reports the user's own
//! input; where an engine has no native console or network source, a hook
//! script in the page's world captures them. A client installs both through
//! its engine and drives the agent with [`call`]; the agent answers with
//! [`Message`]s over the engine's script-message channel.

use serde::Deserialize;
use serde_json::Value;

const AGENT: &str = include_str!("agent.js");
const HOOKS: &str = include_str!("hooks.js");

/// The script-message channel the agent posts to.
pub struct Transport<'a> {
    /// A JavaScript expression, evaluated at document start, for a function
    /// that sends one string to Tcode.
    pub post: &'a str,
    /// Whether the agent runs in the page's own world (Android): the page can
    /// then see it, and the capture hooks run inside the agent's bundle.
    pub page_world: bool,
}

/// What the page agent captures by script, where the engine has no native
/// source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    pub console: bool,
    pub network: bool,
}

/// The scripts to install at document start in the top frame.
#[derive(Debug, PartialEq, Eq)]
pub struct Scripts {
    /// For the agent's world.
    pub agent: String,
    /// For the page's world, when the agent has its own and something is
    /// captured by script.
    pub hooks: Option<String>,
}

pub fn scripts(transport: &Transport<'_>, nonce: &str, capture: Capture) -> Scripts {
    let capture_json = serde_json::json!({
        "console": capture.console,
        "network": capture.network,
    });
    let hooked = capture.console || capture.network;
    if transport.page_world {
        let config = serde_json::json!({ "nonce": nonce, "capture": capture_json });
        let hooks = if hooked { HOOKS } else { "null" };
        return Scripts {
            agent: format!("{AGENT}(({}), {config}, {hooks});", transport.post),
            hooks: None,
        };
    }
    // The hooks hand entries to the agent's world through a DOM event only
    // they and the agent know the name of.
    let event = format!("tcode-{nonce}");
    let config = serde_json::json!({
        "nonce": nonce,
        "hooksEvent": hooked.then_some(event.as_str()),
        "capture": capture_json,
    });
    let event = Value::String(event);
    Scripts {
        agent: format!("{AGENT}(({}), {config}, null);", transport.post),
        hooks: hooked.then(|| {
            format!(
                "(function () {{\n\
                 const dispatch = EventTarget.prototype.dispatchEvent;\n\
                 const Custom = CustomEvent;\n\
                 const stringify = JSON.stringify;\n\
                 const emit = (entry) => {{ try {{ dispatch.call(window, new Custom({event}, {{ detail: stringify(entry) }})); }} catch (_) {{}} }};\n\
                 {HOOKS}(emit, {capture_json});\n\
                 }})();"
            )
        }),
    }
}

/// The script that asks the agent to run `method`; it answers with a
/// [`Message::Reply`] carrying `id`.
pub fn call(id: u64, method: &str, args: &Value) -> String {
    format!(
        "globalThis.__tcodePage && globalThis.__tcodePage.call({id}, {}, {args}); void 0;",
        Value::String(method.to_owned())
    )
}

/// One message from the agent, as it arrives over the channel.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Envelope {
    /// The page's nonce: a message without it did not come from the agent.
    #[serde(rename = "n")]
    pub nonce: String,
    /// The document the agent runs in, fresh for each.
    pub doc: String,
    #[serde(flatten)]
    pub message: Message,
}

impl Envelope {
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Message {
    /// The agent is installed in a new document, at document start.
    Ready {
        url: String,
    },
    /// A call's answer.
    Reply {
        id: u64,
        ok: bool,
        #[serde(default)]
        value: Value,
        #[serde(default)]
        error: Option<String>,
    },
    /// The user acted on the page since the agent's last snapshot.
    Input {
        generation: u64,
    },
    /// The document's URL changed without a new document.
    Url {
        url: String,
    },
    Console(ConsoleEntry),
    Network(NetworkEntry),
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct ConsoleEntry {
    /// `log`, `info`, `warn`, `error` or `debug`.
    pub level: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct NetworkEntry {
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// What asked for it: `fetch`, `xhr`, `script`, `img`, `document`, ….
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Why the request got no response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

impl NetworkEntry {
    /// No response, or an HTTP error.
    pub fn is_failure(&self) -> bool {
        self.failed.is_some() || self.status.is_some_and(|status| status >= 400)
    }
}

/// Wrap an expression the agent asked to evaluate in the page's own world so
/// its value is returned to the engine's callback.
pub fn evaluate(expression: &str) -> String {
    // Parenthesize so an object-literal expression isn't parsed as a block.
    format!("(() => {{ return ({expression}); }})()")
}

/// Decode the string an engine's evaluation callback yields into JSON.
///
/// WKWebView may hand back the value already JSON-encoded, or double-encoded
/// (a JSON string whose contents are themselves JSON). We try to parse, and
/// if we land on a string that itself looks like JSON, parse one more level
/// so callers see structured data rather than an opaque string.
pub fn parse_result(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::String(inner)) => {
            let trimmed = inner.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                serde_json::from_str::<Value>(&inner).unwrap_or(Value::String(inner))
            } else {
                Value::String(inner)
            }
        }
        Ok(value) => value,
        Err(_) => Value::String(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn webview_results_decode_once_or_twice_without_losing_plain_strings() {
        for (raw, expected) in [
            (
                r#"{"url":"https://x","loading":false}"#,
                json!({"url":"https://x","loading":false}),
            ),
            (r#""{\"ok\":true}""#, json!({"ok":true})),
            (r#""hello""#, json!("hello")),
            ("undefined", json!("undefined")),
        ] {
            assert_eq!(parse_result(raw), expected, "{raw}");
        }
    }

    /// What an agent passes reaches the page agent as data: a selector or
    /// text cannot end the call and run as script.
    #[test]
    fn call_arguments_stay_data() {
        let hostile = "\"});alert(1);//\n</script>";
        let script = call(7, "click", &json!({ "selector": hostile }));
        assert!(!script.contains(hostile), "{script}");
        let args = script
            .strip_prefix("globalThis.__tcodePage && globalThis.__tcodePage.call(7, \"click\", ")
            .and_then(|rest| rest.strip_suffix("); void 0;"))
            .expect(&script);
        assert_eq!(
            serde_json::from_str::<Value>(args).unwrap(),
            json!({ "selector": hostile })
        );
    }

    /// The messages agent.js posts, as it writes them.
    #[test]
    fn agent_messages_parse() {
        let cases = [
            (
                r#"{"t":"ready","url":"http://localhost:5173/","n":"k","doc":"d1"}"#,
                Message::Ready {
                    url: "http://localhost:5173/".into(),
                },
            ),
            (
                r#"{"t":"reply","id":3,"ok":true,"value":{"ok":true},"n":"k","doc":"d1"}"#,
                Message::Reply {
                    id: 3,
                    ok: true,
                    value: json!({"ok": true}),
                    error: None,
                },
            ),
            (
                r#"{"t":"reply","id":4,"ok":false,"error":"no element matches selector","n":"k","doc":"d1"}"#,
                Message::Reply {
                    id: 4,
                    ok: false,
                    value: Value::Null,
                    error: Some("no element matches selector".into()),
                },
            ),
            (
                r#"{"t":"input","generation":1,"n":"k","doc":"d1"}"#,
                Message::Input { generation: 1 },
            ),
            (
                r#"{"t":"console","level":"error","text":"boom","location":"http://x/app.js:3","n":"k","doc":"d1"}"#,
                Message::Console(ConsoleEntry {
                    level: "error".into(),
                    text: "boom".into(),
                    location: Some("http://x/app.js:3".into()),
                }),
            ),
            (
                r#"{"t":"network","method":"GET","url":"http://x/api","status":500,"kind":"fetch","ms":12.5,"n":"k","doc":"d1"}"#,
                Message::Network(NetworkEntry {
                    method: "GET".into(),
                    url: "http://x/api".into(),
                    status: Some(500),
                    kind: Some("fetch".into()),
                    ms: Some(12.5),
                    bytes: None,
                    failed: None,
                }),
            ),
        ];
        for (raw, message) in cases {
            let envelope = Envelope::parse(raw).expect(raw);
            assert_eq!(
                (envelope.nonce.as_str(), envelope.doc.as_str()),
                ("k", "d1")
            );
            assert_eq!(envelope.message, message, "{raw}");
        }
    }
}
