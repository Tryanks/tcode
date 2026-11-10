//! The page agent: the script Tcode installs at document start in every
//! preview page, and the messages it and Tcode exchange.
//!
//! The agent keeps refs, builds the accessibility outline, acts on the page
//! with synthetic input, waits on DOM conditions, and reports the user's own
//! input; where an engine has no native console source, a hook script in the
//! page's world captures it. A client installs both through
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

/// The scripts to install at document start in the top frame.
#[derive(Debug, PartialEq, Eq)]
pub struct Scripts {
    /// For the agent's world.
    pub agent: String,
    /// For the page's world, when the agent has its own and the console is
    /// captured by script.
    pub hooks: Option<String>,
}

/// `console`: capture the console by script, where the engine has no
/// native source.
pub fn scripts(transport: &Transport<'_>, nonce: &str, console: bool) -> Scripts {
    if transport.page_world {
        let config = serde_json::json!({ "nonce": nonce });
        let hooks = if console { HOOKS } else { "null" };
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
        "hooksEvent": console.then_some(event.as_str()),
    });
    let event = Value::String(event);
    Scripts {
        agent: format!("{AGENT}(({}), {config}, null);", transport.post),
        hooks: console.then(|| {
            format!(
                "(function () {{\n\
                 const dispatch = EventTarget.prototype.dispatchEvent;\n\
                 const Custom = CustomEvent;\n\
                 const stringify = JSON.stringify;\n\
                 const emit = (entry) => {{ try {{ dispatch.call(window, new Custom({event}, {{ detail: stringify(entry) }})); }} catch (_) {{}} }};\n\
                 {HOOKS}(emit);\n\
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
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct ConsoleEntry {
    /// `log`, `info`, `warn`, `error` or `debug`.
    pub level: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// Wrap an expression the agent asked to evaluate in the page's own world so
/// its value is returned to the engine's callback.
pub fn evaluate(expression: &str) -> String {
    // Parenthesize so an object-literal expression isn't parsed as a block.
    format!("(() => {{ return ({expression}); }})()")
}

/// Decode the JSON an engine's evaluation callback yields; anything that is
/// not JSON, such as wry's empty string for `undefined`, is returned as text.
pub fn parse_result(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// Every message agent.js posts. Shapes are agent.js's own: `send` adds `n` and
    /// `doc` to each; `ready` is the last statement, `reply` is the `reply` helper's
    /// two objects, `input` is sent from `userActed`, `url` from `checkUrl`, and
    /// `console` from `relay`, whose `location` is optional. Contents are invented.
    /// Only a real engine shows that the script still sends these: the preview
    /// smoke run (`--preview-smoke`).
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
                r#"{"t":"url","url":"http://localhost:5173/#settings","n":"k","doc":"d1"}"#,
                Message::Url {
                    url: "http://localhost:5173/#settings".into(),
                },
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
                r#"{"t":"console","level":"error","text":"Unhandled promise rejection: x","n":"k","doc":"d1"}"#,
                Message::Console(ConsoleEntry {
                    level: "error".into(),
                    text: "Unhandled promise rejection: x".into(),
                    location: None,
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
