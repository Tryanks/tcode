//! What happens in one preview page, as its engine and its page agent report
//! it: navigations starting, finishing and failing, the agent's messages,
//! and console entries. A page's calls into the agent wait on
//! these events rather than on time: a call made while a document loads goes
//! to the next document's agent once it is ready, and a wait that spans a
//! navigation follows the page to its next document.
//!
//! The engines feed [`Event`]s in through `page_agent`; the lifecycle owns
//! one [`PageEvents`] per page and carries out each [`Outcome`].

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use preview_mcp::page::{self, ConsoleEntry, Envelope, Message};
use serde_json::Value;

use super::ReplyTx;

/// One failed navigation, as the platform described it. A failed navigation
/// is invisible to the page: WebKit, WebKitGTK and WebView2 keep the previous
/// document, so only the engine can tell an untrusted certificate from a
/// slow dev server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadError {
    pub(crate) url: String,
    /// `NSURLErrorDomain -1202` on macOS, `CertificateIsInvalid` on Windows,
    /// `g-io-error-quark ConnectionRefused` on Linux.
    pub(crate) code: String,
    pub(crate) message: String,
}

impl LoadError {
    pub(crate) fn to_json(&self) -> Value {
        serde_json::json!({
            "url": self.url,
            "code": self.code,
            "message": self.message,
        })
    }

    /// The one-line form used where a tool can only answer with an error.
    pub(crate) fn describe(&self) -> String {
        format!(
            "navigation to {} failed: {} ({})",
            self.url, self.message, self.code
        )
    }
}

/// Something one page's engine reported.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    /// A main-frame navigation began.
    Started,
    /// It finished loading.
    Finished,
    Failed(LoadError),
    /// A string the page agent posted.
    Message(String),
    #[cfg_attr(
        not(any(target_os = "windows", target_os = "android")),
        allow(dead_code)
    )]
    Console(ConsoleEntry),
    /// The page asked for a new window; it loads in this page instead.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    NewWindow(String),
}

/// Where a console entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Source {
    /// The engine's own record.
    Engine,
    /// The page agent's hooks, which miss what they cannot see.
    Script,
}

#[derive(Debug, Clone, serde::Serialize)]
struct Logged<T> {
    #[serde(flatten)]
    entry: T,
    source: Source,
}

/// How many recent errors a status lists.
const RECENT: usize = 5;

pub(crate) type Answer = async_channel::Sender<Result<Value, String>>;

const NO_PAGE: &str = "the preview has no page loaded yet; call preview_open with a URL";
const NO_AGENT: &str = "this page has no page agent (it is not an HTML document); \
                        preview_evaluate and preview_screenshot still work";
const NAVIGATED: &str = "the page navigated before the call was answered; call it again";

/// Whether an agent's URL condition holds for the page's actual URL.
pub(crate) type UrlMatch = Box<dyn Fn(&str) -> bool>;

/// What a wait still needs.
pub(crate) struct WaitFor {
    pub(crate) selector: Option<String>,
    pub(crate) text: Option<String>,
    pub(crate) url: Option<UrlMatch>,
    /// When the wait ends, by the lifecycle's clock.
    pub(crate) deadline: Instant,
}

struct Wait {
    conditions: WaitFor,
    /// Every DOM condition held when the agent last said.
    dom_met: bool,
    /// The DOM conditions the agent last reported unmet.
    pending: Vec<String>,
}

impl Wait {
    fn has_dom(&self) -> bool {
        self.conditions.selector.is_some() || self.conditions.text.is_some()
    }

    fn unmet(&self, url: Option<&str>) -> Vec<String> {
        let mut unmet = if self.dom_met {
            Vec::new()
        } else {
            self.pending.clone()
        };
        if let Some(matches) = &self.conditions.url
            && !url.is_some_and(matches)
        {
            unmet.push("urlIncludes".into());
        }
        unmet
    }
}

struct Call {
    method: &'static str,
    args: Value,
    /// The document it went to; None while it waits for one.
    sent_to: Option<String>,
    answer: Answer,
    wait: Option<Wait>,
}

/// What the lifecycle does after a page's state changed.
#[derive(Default)]
#[must_use]
pub(crate) struct Outcome {
    /// Scripts for the agent's world, in order.
    pub(crate) scripts: Vec<String>,
    /// Evaluations in the page's own world that may run now.
    pub(crate) evaluations: Vec<(String, ReplyTx)>,
    /// The user acted on the page since the agent's last snapshot.
    pub(crate) user_acted: bool,
    /// What the panel shows of the page changed: its load error.
    pub(crate) changed: bool,
}

pub(crate) struct PageEvents {
    nonce: String,
    load_error: Option<LoadError>,
    /// The document whose agent last said it was ready.
    doc: Option<String>,
    /// That document's URL as its agent last reported it.
    url: Option<String>,
    /// A navigation started and has not committed, finished or failed.
    navigating: bool,
    next_call: u64,
    calls: HashMap<u64, Call>,
    /// Page-world evaluations waiting for the navigation to settle.
    deferred: Vec<(String, ReplyTx)>,
    console: Console,
}

/// The console since the page's last navigation: every entry counted, the
/// latest errors kept.
#[derive(Default)]
struct Console {
    entries: usize,
    warnings: usize,
    errors: usize,
    recent_errors: VecDeque<Logged<ConsoleEntry>>,
}

impl Console {
    fn record(&mut self, logged: Logged<ConsoleEntry>) {
        self.entries += 1;
        match logged.entry.level.as_str() {
            "warn" => self.warnings += 1,
            "error" => {
                self.errors += 1;
                if self.recent_errors.len() == RECENT {
                    self.recent_errors.pop_front();
                }
                self.recent_errors.push_back(logged);
            }
            _ => {}
        }
    }
}

impl PageEvents {
    /// A new page; `loading` when the engine already loads its first
    /// document.
    pub(crate) fn new(nonce: String, loading: bool) -> Self {
        Self {
            nonce,
            load_error: None,
            doc: None,
            url: None,
            navigating: loading,
            next_call: 0,
            calls: HashMap::new(),
            deferred: Vec::new(),
            console: Console::default(),
        }
    }

    pub(crate) fn load_error(&self) -> Option<&LoadError> {
        self.load_error.as_ref()
    }

    /// Whether the agent can answer now: a document is ready and no
    /// navigation is replacing it.
    pub(crate) fn settled(&self) -> bool {
        self.doc.is_some() && !self.navigating
    }

    pub(crate) fn navigating(&self) -> bool {
        self.navigating
    }

    /// Tcode loads a URL: until the next document is ready, calls wait.
    pub(crate) fn navigate(&mut self, now: Instant) -> Outcome {
        self.handle(Event::Started, now)
    }

    /// What `event` does to the page at `now`, the lifecycle's time.
    pub(crate) fn handle(&mut self, event: Event, now: Instant) -> Outcome {
        let mut outcome = Outcome::default();
        match event {
            Event::Started => {
                self.navigating = true;
                outcome.changed = self.load_error.take().is_some();
                self.console = Console::default();
            }
            Event::Finished => {
                self.navigating = false;
                self.flush(&mut outcome, now);
            }
            Event::Failed(error) => {
                self.navigating = false;
                let failure = error.describe();
                self.load_error = Some(error);
                outcome.changed = true;
                let waits: Vec<u64> = self
                    .calls
                    .iter()
                    .filter(|(_, call)| call.wait.is_some())
                    .map(|(id, _)| *id)
                    .collect();
                for id in waits {
                    if let Some(call) = self.calls.remove(&id) {
                        let _ = call.answer.try_send(Err(failure.clone()));
                    }
                }
                self.flush(&mut outcome, now);
            }
            Event::Message(raw) => self.message(&raw, &mut outcome, now),
            Event::Console(entry) => self.console.record(Logged {
                entry,
                source: Source::Engine,
            }),
            // The lifecycle loads it.
            Event::NewWindow(_) => {}
        }
        outcome
    }

    fn message(&mut self, raw: &str, outcome: &mut Outcome, now: Instant) {
        let Some(envelope) = Envelope::parse(raw) else {
            log::debug!("preview: unreadable page agent message {raw:.200}");
            return;
        };
        if envelope.nonce != self.nonce {
            log::debug!("preview: dropped a page message without the agent's nonce");
            return;
        }
        match envelope.message {
            Message::Ready { url } => {
                self.url = Some(url);
                if self.doc.as_deref() != Some(envelope.doc.as_str()) {
                    self.doc = Some(envelope.doc.clone());
                    self.replaced(&envelope.doc);
                }
                self.navigating = false;
                self.check_urls();
                self.flush(outcome, now);
            }
            Message::Reply {
                id,
                ok,
                value,
                error,
            } => self.reply(id, ok, value, error),
            Message::Input { .. } => outcome.user_acted = true,
            Message::Url { url } => {
                if self.doc.as_deref() == Some(envelope.doc.as_str()) {
                    self.url = Some(url);
                    self.check_urls();
                }
            }
            Message::Console(entry) => self.console.record(Logged {
                entry,
                source: Source::Script,
            }),
        }
    }

    /// A new document's agent is ready: calls the old one never answered
    /// fail, and waits move to the new one.
    fn replaced(&mut self, doc: &str) {
        let stale: Vec<u64> = self
            .calls
            .iter()
            .filter(|(_, call)| call.sent_to.as_deref().is_some_and(|to| to != doc))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            let Some(mut call) = self.calls.remove(&id) else {
                continue;
            };
            match call.wait.as_mut() {
                Some(wait) => {
                    wait.dom_met = false;
                    call.sent_to = None;
                    self.calls.insert(id, call);
                }
                None => {
                    let _ = call.answer.try_send(Err(NAVIGATED.into()));
                }
            }
        }
    }

    fn reply(&mut self, id: u64, ok: bool, value: Value, error: Option<String>) {
        let Some(mut call) = self.calls.remove(&id) else {
            return;
        };
        let Some(wait) = call.wait.as_mut() else {
            let _ = call.answer.try_send(if ok {
                Ok(value)
            } else {
                Err(error.unwrap_or_else(|| "the page agent failed".into()))
            });
            return;
        };
        if !ok {
            let _ = call
                .answer
                .try_send(Err(error.unwrap_or_else(|| "the wait failed".into())));
            return;
        }
        wait.pending = strings(&value["pending"]);
        wait.dom_met = value["matched"].as_bool() == Some(true);
        let unmet = wait.unmet(self.url.as_deref());
        if unmet.is_empty() {
            let _ = call.answer.try_send(Ok(self.matched()));
        } else if !wait.dom_met {
            // The agent's deadline is the wait's.
            let _ = call.answer.try_send(Err(wait_timeout_message(&unmet)));
        } else {
            // The DOM holds; the URL does not yet.
            self.calls.insert(id, call);
        }
    }

    fn matched(&self) -> Value {
        serde_json::json!({
            "matched": true,
            "url": self.url,
            "pending": [],
        })
    }

    /// Answer waits whose last condition was the page's URL.
    fn check_urls(&mut self) {
        let met: Vec<u64> = self
            .calls
            .iter()
            .filter(|(_, call)| {
                call.wait.as_ref().is_some_and(|wait| {
                    wait.conditions.url.is_some()
                        && (wait.dom_met || !wait.has_dom())
                        && wait.unmet(self.url.as_deref()).is_empty()
                })
            })
            .map(|(id, _)| *id)
            .collect();
        for id in met {
            if let Some(call) = self.calls.remove(&id) {
                let _ = call.answer.try_send(Ok(self.matched()));
            }
        }
    }

    /// Why a call has no document to run in: the navigation's own error
    /// when the last load failed, `otherwise` when none did.
    fn no_document(&self, otherwise: &str) -> String {
        self.load_error
            .as_ref()
            .map_or_else(|| otherwise.to_owned(), LoadError::describe)
    }

    /// Send what waited for the page to settle.
    fn flush(&mut self, outcome: &mut Outcome, now: Instant) {
        if self.navigating {
            return;
        }
        outcome.evaluations.append(&mut self.deferred);
        let waiting: Vec<u64> = self
            .calls
            .iter()
            .filter(|(_, call)| call.sent_to.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in waiting {
            self.send(id, outcome, now);
        }
    }

    fn send(&mut self, id: u64, outcome: &mut Outcome, now: Instant) {
        let Some(doc) = self.doc.clone() else {
            if let Some(call) = self.calls.remove(&id) {
                let _ = call.answer.try_send(Err(self.no_document(NO_AGENT)));
            }
            return;
        };
        let Some(call) = self.calls.get_mut(&id) else {
            return;
        };
        let args = match &call.wait {
            Some(wait) if !wait.has_dom() => {
                // Only the URL is awaited, which the page reports unasked.
                call.sent_to = Some(doc);
                return;
            }
            Some(wait) => {
                let remaining = wait.conditions.deadline.saturating_duration_since(now);
                serde_json::json!({
                    "selector": wait.conditions.selector,
                    "text": wait.conditions.text,
                    "timeoutMs": remaining.as_millis() as u64,
                })
            }
            None => call.args.clone(),
        };
        call.sent_to = Some(doc);
        outcome.scripts.push(page::call(id, call.method, &args));
    }

    fn register(
        &mut self,
        method: &'static str,
        args: Value,
        answer: Answer,
        wait: Option<Wait>,
        now: Instant,
    ) -> (u64, Outcome) {
        self.next_call += 1;
        let id = self.next_call;
        let mut outcome = Outcome::default();
        if self.doc.is_none() && !self.navigating {
            let _ = answer.try_send(Err(self.no_document(NO_PAGE)));
            return (id, outcome);
        }
        self.calls.insert(
            id,
            Call {
                method,
                args,
                sent_to: None,
                answer,
                wait,
            },
        );
        if !self.navigating {
            self.send(id, &mut outcome, now);
        }
        (id, outcome)
    }

    /// Ask the agent to run `method`; `answer` gets its value.
    pub(crate) fn call(
        &mut self,
        method: &'static str,
        args: Value,
        answer: Answer,
        now: Instant,
    ) -> (u64, Outcome) {
        self.register(method, args, answer, None, now)
    }

    /// Wait until every condition holds or the deadline passes.
    pub(crate) fn wait(
        &mut self,
        conditions: WaitFor,
        answer: Answer,
        now: Instant,
    ) -> (u64, Outcome) {
        let wait = Wait {
            pending: [
                conditions.selector.as_ref().map(|_| "selector".to_owned()),
                conditions.text.as_ref().map(|_| "text".to_owned()),
            ]
            .into_iter()
            .flatten()
            .collect(),
            dom_met: false,
            conditions,
        };
        if let Some(error) = &self.load_error
            && self.doc.is_none()
        {
            let _ = answer.try_send(Err(error.describe()));
            return (0, Outcome::default());
        }
        if !wait.has_dom() && wait.unmet(self.url.as_deref()).is_empty() && !self.navigating {
            let _ = answer.try_send(Ok(self.matched()));
            return (0, Outcome::default());
        }
        self.register("wait", Value::Null, answer, Some(wait), now)
    }

    /// Nobody waits for the call's answer any more.
    pub(crate) fn forget(&mut self, id: u64) {
        self.calls.remove(&id);
    }

    /// The wait's time is up: it says what is still unmet.
    pub(crate) fn expire(&mut self, id: u64) {
        let Some(Call {
            wait: Some(wait),
            answer,
            ..
        }) = self.calls.remove(&id)
        else {
            return;
        };
        let _ = answer.try_send(Err(wait_timeout_message(&wait.unmet(self.url.as_deref()))));
    }

    /// Run `script` in the page's own world once no navigation is under way.
    pub(crate) fn evaluate(&mut self, script: String, reply: ReplyTx) -> Outcome {
        let mut outcome = Outcome::default();
        if self.navigating {
            self.deferred.push((script, reply));
        } else {
            outcome.evaluations.push((script, reply));
        }
        outcome
    }

    /// The page is going away: every caller hears so now.
    pub(crate) fn close(&mut self) {
        for (_, call) in self.calls.drain() {
            let _ = call
                .answer
                .try_send(Err("preview browser was closed".into()));
        }
        for (_, reply) in self.deferred.drain(..) {
            let _ = reply.try_send(Err("preview browser was closed".into()));
        }
    }

    /// What `preview_status` adds of the console since the page's last
    /// navigation.
    pub(crate) fn summary(&self) -> Value {
        serde_json::json!({
            "console": {
                "entries": self.console.entries,
                "errors": self.console.errors,
                "warnings": self.console.warnings,
                "recentErrors": self.console.recent_errors,
            },
        })
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn wait_timeout_message(pending: &[String]) -> String {
    format!(
        "preview_wait_for timed out; unmet conditions: {}",
        pending.join(", ")
    )
}

/// Android's page events, as `PreviewHost` numbers them. Errors apply only
/// to the main frame, and an error document's own finish must not clear the
/// failure: Android also finishes the generated error document.
#[cfg(any(target_os = "android", test))]
pub(super) fn android_event(kind: i32, url: String, code: i32, message: String) -> Option<Event> {
    match kind {
        0 => Some(Event::Started),
        1 => Some(Event::Finished),
        3..=6 => {
            let domain = match kind {
                3 => "WebView",
                4 => "HTTP",
                5 => "SSL",
                _ => "Android",
            };
            Some(Event::Failed(LoadError {
                url,
                code: format!("{domain} {code}"),
                message,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The moment the page's events happen at in these tests: an opaque
    /// origin, which no test moves.
    fn at() -> Instant {
        static AT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *AT.get_or_init(Instant::now)
    }

    fn message(doc: &str, body: Value) -> Event {
        let mut body = body;
        body["n"] = "nonce".into();
        body["doc"] = doc.into();
        Event::Message(body.to_string())
    }

    fn ready(doc: &str, url: &str) -> Event {
        message(doc, json!({"t": "ready", "url": url}))
    }

    fn sent_ids(outcome: &Outcome) -> Vec<u64> {
        outcome
            .scripts
            .iter()
            .map(|script| {
                let rest = script
                    .strip_prefix("globalThis.__tcodePage && globalThis.__tcodePage.call(")
                    .unwrap();
                rest[..rest.find(',').unwrap()].parse().unwrap()
            })
            .collect()
    }

    fn page_on(url: &str) -> PageEvents {
        let mut page = PageEvents::new("nonce".into(), true);
        let _ = page.handle(ready("d1", url), at());
        let _ = page.handle(Event::Finished, at());
        page
    }

    /// A snapshot asked for right after a navigation reads the new
    /// document, once its agent is ready, not the one being replaced.
    #[test]
    fn calls_during_a_navigation_go_to_the_next_document() {
        let mut page = page_on("http://localhost/a");
        let _ = page.navigate(at());
        let (answer, answered) = async_channel::bounded(1);
        let (id, outcome) = page.call("snapshot", json!({}), answer, at());
        assert!(outcome.scripts.is_empty());

        let outcome = page.handle(ready("d2", "http://localhost/b"), at());
        assert_eq!(sent_ids(&outcome), [id]);
        let _ = page.handle(
            message(
                "d2",
                json!({"t": "reply", "id": id, "ok": true, "value": {"title": "B"}}),
            ),
            at(),
        );
        assert_eq!(answered.try_recv().unwrap(), Ok(json!({"title": "B"})));
    }

    /// A document replaced while a call was out fails the call instead of
    /// leaving it to time out; a wait moves to the new document.
    #[test]
    fn a_new_document_fails_calls_and_rearms_waits() {
        let mut page = page_on("http://localhost/a");
        let (click, clicked) = async_channel::bounded(1);
        let (_, _) = page.call("click", json!({"ref": "e1"}), click, at());
        let (wait, waited) = async_channel::bounded(1);
        let (wait_id, _) = page.wait(
            WaitFor {
                selector: None,
                text: Some("Saved".into()),
                url: None,
                deadline: at() + std::time::Duration::from_secs(30),
            },
            wait,
            at(),
        );

        let _ = page.handle(Event::Started, at());
        let outcome = page.handle(ready("d2", "http://localhost/saved"), at());
        assert_eq!(clicked.try_recv().unwrap(), Err(NAVIGATED.into()));
        assert_eq!(sent_ids(&outcome), [wait_id]);
        let _ = page.handle(message(
            "d2",
            json!({"t": "reply", "id": wait_id, "ok": true, "value": {"matched": true, "pending": []}}),
        ), at());
        assert_eq!(waited.try_recv().unwrap().unwrap()["matched"], true);
    }

    /// A wait for a URL ends when a document at that URL is ready, and a
    /// failed navigation ends a wait with the engine's error.
    #[test]
    fn url_waits_follow_navigations_and_load_errors_end_waits() {
        let mut page = page_on("http://localhost/");
        let (wait, waited) = async_channel::bounded(1);
        let _ = page.wait(
            WaitFor {
                selector: None,
                text: None,
                url: Some(Box::new(|url: &str| url.contains("/done"))),
                deadline: at() + std::time::Duration::from_secs(30),
            },
            wait,
            at(),
        );
        let _ = page.handle(
            message("d1", json!({"t": "url", "url": "http://localhost/#step"})),
            at(),
        );
        assert!(waited.try_recv().is_err());
        let _ = page.handle(ready("d2", "http://localhost/done"), at());
        assert_eq!(
            waited.try_recv().unwrap().unwrap()["url"],
            "http://localhost/done"
        );

        let (wait, waited) = async_channel::bounded(1);
        let _ = page.wait(
            WaitFor {
                selector: Some("#never".into()),
                text: None,
                url: None,
                deadline: at() + std::time::Duration::from_secs(30),
            },
            wait,
            at(),
        );
        let failure = LoadError {
            url: "https://localhost:8443/".into(),
            code: "NSURLErrorDomain -1202".into(),
            message: "The certificate for this server is invalid.".into(),
        };
        let outcome = page.handle(Event::Failed(failure), at());
        assert!(outcome.changed);
        assert_eq!(
            waited.try_recv().unwrap(),
            Err("navigation to https://localhost:8443/ failed: \
                 The certificate for this server is invalid. (NSURLErrorDomain -1202)"
                .into())
        );
    }

    /// Messages a page could forge without the agent's nonce are dropped;
    /// the user's input is reported.
    #[test]
    fn only_the_agent_reports_user_input() {
        let mut page = page_on("http://localhost/");
        let forged = json!({"t": "input", "generation": 1, "n": "guess", "doc": "d1"});
        assert!(
            !page
                .handle(Event::Message(forged.to_string()), at())
                .user_acted
        );
        assert!(
            page.handle(message("d1", json!({"t": "input", "generation": 1})), at())
                .user_acted
        );
    }

    /// The status counts every console entry since the page's last
    /// navigation, however chatty the page; an early error stays counted.
    #[test]
    fn the_console_summary_counts_every_entry() {
        let mut page = page_on("http://localhost/");
        let console =
            |level: &str| message("d1", json!({"t": "console", "level": level, "text": "x"}));
        let _ = page.handle(console("error"), at());
        for _ in 0..1000 {
            let _ = page.handle(console("log"), at());
        }
        let summary = page.summary();
        assert_eq!(summary["console"]["entries"], 1001);
        assert_eq!(summary["console"]["errors"], 1);
        assert_eq!(
            summary["console"]["recentErrors"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn android_failure_survives_error_document_finish() {
        let mut page = page_on("https://example.com/");
        for (kind, code, expected) in [(3, -2, "WebView -2"), (4, 503, "HTTP 503"), (5, 3, "SSL 3")]
        {
            for event in [
                android_event(0, "https://dead.invalid/".into(), 0, String::new()),
                android_event(kind, "https://dead.invalid/".into(), code, "failure".into()),
                android_event(1, "https://dead.invalid/".into(), 0, String::new()),
            ] {
                let _ = page.handle(event.unwrap(), at());
            }
            assert_eq!(
                page.load_error().unwrap().to_json(),
                json!({"url": "https://dead.invalid/", "code": expected, "message": "failure"})
            );
        }
        let _ = page.handle(
            android_event(0, "https://example.com/".into(), 0, String::new()).unwrap(),
            at(),
        );
        assert_eq!(page.load_error(), None);
    }
}
