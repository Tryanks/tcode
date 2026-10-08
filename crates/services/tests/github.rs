use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime},
};
use tcode_core::settings::{GitHubCredentialSource, GitHubHostSettings};
use tcode_services::{
    github::{
        CredentialError, Credentials, GitHubApi, GitHubError, RequestOptions, RestRequest,
        api::Authentication,
        graphql::{self, AliasItem, Document},
    },
    settings::SettingsStore,
};

// The agent's documented TLS/DNS interfaces route real HTTPS requests into a loopback
// HTTP fixture without a certificate dependency or an endpoint override in production.
struct LoopbackTls;
impl ureq::TlsConnector for LoopbackTls {
    fn connect(
        &self,
        _: &str,
        io: Box<dyn ureq::ReadWrite>,
    ) -> Result<Box<dyn ureq::ReadWrite>, ureq::Error> {
        Ok(io)
    }
}
struct Exchange {
    request: String,
    body: Vec<u8>,
    stream: TcpStream,
}
impl Exchange {
    fn reply(mut self, status: u16, headers: &str, body: &[u8]) {
        write!(
            self.stream,
            "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .unwrap();
        let _ = self.stream.write_all(body);
    }
}
struct Fixture {
    address: std::net::SocketAddr,
    incoming: mpsc::Receiver<Exchange>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, incoming) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            'connections: for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        continue 'connections;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let length = request
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                if tx
                    .send(Exchange {
                        request,
                        body,
                        stream,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            address,
            incoming,
            stop,
            thread: Some(thread),
        }
    }
    fn builder(&self) -> ureq::AgentBuilder {
        let address = self.address;
        ureq::AgentBuilder::new()
            .resolver(move |_: &str| Ok(vec![address]))
            .tls_connector(Arc::new(LoopbackTls))
    }
    fn next(&self) -> Exchange {
        self.incoming
            .recv_timeout(Duration::from_secs(5))
            .expect("client sent request to fixture")
    }
    fn call<T: Send>(
        &self,
        run: impl FnOnce() -> T + Send,
        status: u16,
        headers: &str,
        body: &[u8],
    ) -> (T, String, Vec<u8>) {
        thread::scope(|scope| {
            let job = scope.spawn(run);
            let exchange = self.next();
            let request = exchange.request.clone();
            let sent = exchange.body.clone();
            exchange.reply(status, headers, body);
            (job.join().unwrap(), request, sent)
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}
struct Store {
    store: SettingsStore,
    root: std::path::PathBuf,
}
impl Store {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("tcode-github-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self {
            store: SettingsStore::new(root.clone()),
            root,
        }
    }
    fn credentials(&self, environment: &[(&str, &str)]) -> Arc<Credentials> {
        Credentials::new(
            self.store.clone(),
            environment
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        )
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
fn quota(resource: &str, remaining: u64, reset: u64) -> String {
    format!(
        "x-ratelimit-resource: {resource}\r\nx-ratelimit-limit: 100\r\nx-ratelimit-remaining: {remaining}\r\nx-ratelimit-reset: {reset}\r\n"
    )
}
fn reset() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600
}
fn document() -> Document {
    Document {
        query: "query Fixture { viewer { login } }".into(),
        variables: BTreeMap::new(),
    }
}

#[test]
fn maps_hosts_and_keeps_saved_environment_and_enterprise_credentials_in_their_boundaries() {
    let fixture = Fixture::new();
    let store = Store::new();
    let credentials = store.credentials(&[
        ("GH_TOKEN", "public-first"),
        ("GITHUB_TOKEN", "public-second"),
        ("GH_ENTERPRISE_TOKEN", "enterprise-first"),
        ("GITHUB_ENTERPRISE_TOKEN", "enterprise-second"),
        ("GH_HOST", " GIT.EXAMPLE.COM "),
    ]);
    let api = GitHubApi::new(credentials.clone(), fixture.builder());
    for (host, domain, path, token) in [
        (" GITHUB.COM ", "api.github.com", "/user", "public-first"),
        (
            "tenant.ghe.com",
            "api.tenant.ghe.com",
            "/user",
            "public-first",
        ),
        (
            "git.example.com",
            "git.example.com",
            "/api/v3/user",
            "enterprise-first",
        ),
    ] {
        let (answer, request, _) = fixture.call(
            || api.rest(host, RestRequest::get("/user"), &RequestOptions::default()),
            200,
            "",
            b"{}",
        );
        answer.unwrap();
        assert!(request.starts_with(&format!("GET {path} ")));
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("host: {domain}\r\n"))
        );
        assert!(request.contains(&format!("Bearer {token}")));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("user-agent: tcode\r\n")
        );
    }
    let (answer, request, _) = fixture.call(
        || api.graphql("git.example.com", &document(), &RequestOptions::default()),
        200,
        "",
        b"{\"data\":{}}",
    );
    answer.unwrap();
    assert!(request.starts_with("POST /api/graphql "));
    store
        .store
        .set_github_token("GITHUB.COM", Some("saved-first"))
        .unwrap();
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer saved-first"));
    store
        .store
        .set_github_token("github.com", Some("replacement"))
        .unwrap();
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer replacement"));
    store.store.set_github_token("github.com", None).unwrap();
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer public-first"));
    credentials.configure(BTreeMap::from([(
        "github.com".into(),
        GitHubHostSettings {
            enabled: false,
            account: None,
        },
    )]));
    assert_eq!(
        api.rest(
            "github.com",
            RestRequest::get("/user"),
            &RequestOptions::default()
        )
        .unwrap_err(),
        GitHubError::Credential(CredentialError::Disabled)
    );
    // Anonymous release checks never read a saved token or the disabled switch.
    let (answer, request, _) = fixture.call(
        || tcode_services::version_check::fetch_latest_tcode_release_json(&api),
        200,
        "",
        b"{\"tag_name\":\"v99.0.0\"}",
    );
    answer.unwrap();
    assert!(request.starts_with("GET /repos/Tryanks/tcode/releases/latest "));
    assert!(!request.to_ascii_lowercase().contains("authorization"));
}

#[test]
fn classifies_graphql_partial_errors_rate_limits_and_rest_errors_before_http_success() {
    for (status, headers, body, expected) in [
        (
            200,
            "",
            r#"{"data":{"viewer":{}},"errors":[{"message":"bad"}]}"#,
            "response",
        ),
        (
            200,
            "",
            r#"{"errors":[{"type":"RATE_LIMITED"}]}"#,
            "limited",
        ),
        (
            200,
            "x-ratelimit-remaining: 0\r\n",
            r#"{"errors":[{"message":"quota exhausted"}]}"#,
            "limited",
        ),
        (
            200,
            "",
            r#"{"errors":[{"message":"API rate limit already exceeded"}]}"#,
            "limited",
        ),
        (
            200,
            "",
            r#"{"errors":[{"message":"API rate limit exceeded"}]}"#,
            "limited",
        ),
        (
            401,
            "",
            r#"{"errors":[{"type":"RATE_LIMITED"}]}"#,
            "limited",
        ),
        (200, "", r#"{"errors":[{"type":"NOT_FOUND"}]}"#, "missing"),
        (
            200,
            "",
            r#"{"errors":[{"type":"NOT_FOUND"},{"type":"FORBIDDEN"}]}"#,
            "response",
        ),
    ] {
        let fixture = Fixture::new();
        let store = Store::new();
        let api = GitHubApi::new(
            store.credentials(&[("GH_TOKEN", "fixture")]),
            fixture.builder(),
        );
        let (answer, _, _) = fixture.call(
            || api.graphql("github.com", &document(), &RequestOptions::default()),
            status,
            headers,
            body.as_bytes(),
        );
        match expected {
            "response" => assert!(matches!(answer, Err(GitHubError::Response { .. }))),
            "limited" => {
                assert!(matches!(answer, Err(GitHubError::RateLimited { .. })));
                assert!(matches!(
                    api.rest(
                        "github.com",
                        RestRequest::get("/user"),
                        &RequestOptions::default()
                    ),
                    Err(GitHubError::Paused { .. })
                ));
            }
            "missing" => assert!(matches!(answer, Err(GitHubError::NotFound))),
            _ => unreachable!(),
        }
    }
    for (status, headers, body, limited) in [
        (429, "", "", true),
        (403, "x-ratelimit-remaining: 0\r\n", "", true),
        (403, "retry-after: 60\r\n", "", true),
        (403, "", "rate limit", true),
        (
            403,
            "",
            r#"{"message":"forbidden","errors":["detail",{"field":"ref","code":"invalid"}]}"#,
            false,
        ),
    ] {
        let fixture = Fixture::new();
        let store = Store::new();
        let api = GitHubApi::new(
            store.credentials(&[("GH_TOKEN", "fixture")]),
            fixture.builder(),
        );
        let (answer, _, _) = fixture.call(
            || {
                api.rest(
                    "github.com",
                    RestRequest::get("/user"),
                    &RequestOptions::default(),
                )
            },
            status,
            headers,
            body.as_bytes(),
        );
        if limited {
            assert!(matches!(answer, Err(GitHubError::RateLimited { .. })));
        } else {
            assert_eq!(
                answer.unwrap_err(),
                GitHubError::Response {
                    status: 403,
                    messages: vec!["forbidden".into(), "detail".into(), "ref invalid".into()]
                }
            );
        }
    }
}

#[test]
fn conditional_reads_reuse_the_validator_without_debiting_and_reserve_stays_interactive() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let window = reset();
    let headers = quota("core", 10, window) + "etag: \"same\"\r\n";
    let (first, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/checks"),
                &RequestOptions::default(),
            )
        },
        200,
        &headers,
        b"cached checks",
    );
    let first = first.unwrap();
    for _ in 0..12 {
        let mut request = RestRequest::get("/checks");
        request.if_none_match = first.headers.get("etag").map(String::as_str);
        let (answer, sent, _) = fixture.call(
            || api.rest("github.com", request, &RequestOptions::default()),
            304,
            "",
            b"",
        );
        assert_eq!(answer.unwrap().status, 304);
        assert!(
            sent.to_ascii_lowercase()
                .contains("if-none-match: \"same\"")
        );
    }
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/checks"),
                &RequestOptions::default(),
            )
        },
        304,
        "",
        b"",
    );
    assert!(matches!(
        answer,
        Err(GitHubError::Response { status: 304, .. })
    ));
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/checks"),
                &RequestOptions::default(),
            )
        },
        200,
        &quota("core", 9, window),
        b"{}",
    );
    answer.unwrap();
    assert!(matches!(
        api.rest(
            "github.com",
            RestRequest::get("/checks"),
            &RequestOptions::default()
        ),
        Err(GitHubError::Paused { .. })
    ));
    let interactive = RequestOptions {
        interactive: true,
        ..Default::default()
    };
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/checks"), &interactive),
        200,
        &quota("core", 1, window),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/checks"), &interactive),
        200,
        &quota("core", 0, window),
        b"{}",
    );
    answer.unwrap();
    assert!(matches!(
        api.rest("github.com", RestRequest::get("/checks"), &interactive),
        Err(GitHubError::Paused { .. })
    ));
    // A different resource and anonymous credential have independent quota snapshots.
    let (answer, _, _) = fixture.call(
        || api.graphql("github.com", &document(), &RequestOptions::default()),
        200,
        "",
        b"{\"data\":{}}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || tcode_services::version_check::fetch_latest_tcode_release_json(&api),
        200,
        "",
        b"{}",
    );
    answer.unwrap();
}

#[test]
fn latest_arrival_can_restore_quota_but_an_older_success_cannot_clear_a_new_refusal() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let window = reset();
    thread::scope(|scope| {
        let old = scope.spawn(|| {
            api.rest(
                "github.com",
                RestRequest::get("/old"),
                &RequestOptions::default(),
            )
        });
        let old_exchange = fixture.next();
        let (answer, _, _) = fixture.call(
            || api.graphql("github.com", &document(), &RequestOptions::default()),
            200,
            "retry-after: 600\r\n",
            b"{\"errors\":[{\"type\":\"RATE_LIMITED\"}]}",
        );
        assert!(matches!(answer, Err(GitHubError::RateLimited { .. })));
        old_exchange.reply(200, &quota("core", 90, window), b"{}");
        old.join().unwrap().unwrap();
    });
    assert!(matches!(
        api.rest(
            "github.com",
            RestRequest::get("/next"),
            &RequestOptions::default()
        ),
        Err(GitHubError::Paused { .. })
    ));
    let interactive = RequestOptions {
        interactive: true,
        ..Default::default()
    };
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/next"), &interactive),
        200,
        &quota("core", 1, window),
        b"{}",
    );
    answer.unwrap();
    assert!(matches!(
        api.rest(
            "github.com",
            RestRequest::get("/next"),
            &RequestOptions::default()
        ),
        Err(GitHubError::Paused { .. })
    ));
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/next"), &interactive),
        200,
        &quota("core", 70, window),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/next"), &interactive),
        200,
        &quota("core", 0, window - 1),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/next"), &interactive),
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    store
        .store
        .set_github_token("github.com", Some("fresh-quota-scope"))
        .unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/new"), &interactive),
        200,
        &quota("core", 1, window),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || api.rest("github.com", RestRequest::get("/new"), &interactive),
        200,
        &quota("core", 70, window),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/new"),
                &RequestOptions::default(),
            )
        },
        200,
        &quota("core", 0, window - 1),
        b"{}",
    );
    answer.unwrap();
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/new"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
}

#[test]
fn pinned_verified_identity_is_cached_per_fingerprint_and_refuses_another_host() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let (verified, sent, _) = fixture.call(
        || api.verified_credential("github.com"),
        200,
        "",
        b"{\"id\":42,\"login\":\"fixture-user\"}",
    );
    assert!(sent.starts_with("GET /user "));
    let (credential, identity) = verified.unwrap();
    assert_eq!(identity.login, "fixture-user");
    assert_eq!(api.verified_credential("github.com").unwrap().1, identity);
    let options = RequestOptions {
        authentication: Authentication::Pinned(credential),
        ..Default::default()
    };
    assert_eq!(
        api.rest("other.example.com", RestRequest::get("/user"), &options)
            .unwrap_err(),
        GitHubError::Credential(CredentialError::HostMismatch)
    );
    store
        .store
        .set_github_token("github.com", Some("replacement"))
        .unwrap();
    let (verified, _, _) = fixture.call(
        || api.verified_credential("github.com"),
        200,
        "",
        b"{\"id\":43,\"login\":\"other-user\"}",
    );
    assert_eq!(verified.unwrap().1.id, 43);
}

#[test]
fn body_cap_and_deadline_cover_streaming_after_headers() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/large"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        &vec![b'a'; 8 * 1024 * 1024 + 1],
    );
    let response = answer.unwrap();
    assert!(response.truncated);
    assert_eq!(response.body.len(), 8 * 1024 * 1024);
    assert_eq!(
        response.json::<serde_json::Value>().unwrap_err(),
        GitHubError::BodyTooLarge
    );
    let bounded = RequestOptions {
        body_limit: 16,
        ..Default::default()
    };
    let (answer, _, _) = fixture.call(
        || api.graphql("github.com", &document(), &bounded),
        200,
        "",
        b"{\"data\":{}}                 {\"errors\":[{}]}",
    );
    assert_eq!(answer.unwrap_err(), GitHubError::BodyTooLarge);
    let (answer, _, _) = fixture.call(
        || api.graphql("github.com", &document(), &bounded),
        429,
        "",
        b"a refusal whose body exceeds the cap",
    );
    assert!(matches!(
        answer,
        Err(GitHubError::RateLimited { status: 429, .. })
    ));
    let (answer, _, _) = fixture.call(
        || tcode_services::version_check::fetch_latest_tcode_release_json(&api),
        200,
        "",
        &vec![b'a'; 1024 * 1024 + 1],
    );
    assert_eq!(
        answer.unwrap_err(),
        tcode_services::version_check::FetchError::ResponseTooLarge
    );
    let timeout = RequestOptions {
        timeout: Duration::from_millis(200),
        interactive: true,
        ..Default::default()
    };
    thread::scope(|scope| {
        let job = scope.spawn(|| api.rest("github.com", RestRequest::get("/slow"), &timeout));
        let mut exchange = fixture.next();
        exchange
            .stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
            .unwrap();
        // Keep the body open until the client reports its whole-response deadline.
        assert_eq!(job.join().unwrap().unwrap_err(), GitHubError::Deadline);
    });
}

#[test]
fn aliases_and_pager_send_values_as_variables_and_report_unread_tail() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let hostile = "owner\") { secret } #";
    let doc = graphql::aliases(
        "query",
        "Batch",
        "r",
        &[
            AliasItem {
                key: 0,
                variables: BTreeMap::from([(
                    "owner".into(),
                    ("String!".into(), serde_json::json!(hostile)),
                )]),
            },
            AliasItem {
                key: 1,
                variables: BTreeMap::from([(
                    "owner".into(),
                    ("String!".into(), serde_json::json!("other")),
                )]),
            },
        ],
        &BTreeMap::from([(
            "name".into(),
            ("String!".into(), serde_json::json!("shared-repo")),
        )]),
        |vars| format!("repository(owner: {}, name: $name) {{ id }}", vars["owner"]),
        |fields| fields,
    )
    .unwrap();
    let (answer, _, body) = fixture.call(
        || api.graphql("github.com", &doc, &RequestOptions::default()),
        200,
        "",
        b"{\"data\":{}}",
    );
    answer.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let query = sent["query"].as_str().unwrap();
    assert!(!query.contains(hostile));
    assert!(!query.contains("shared-repo"));
    assert_eq!(sent["variables"]["r0_owner"], hostile);
    assert_eq!(query.matches("$name:").count(), 1);
    for (cursors, max_pages, truncated) in [
        (vec![Some("a"), None], None, false),
        (vec![Some("a"), Some("a")], None, true),
        (vec![Some("a")], Some(1), true),
    ] {
        thread::scope(|scope| {
            let job = scope.spawn(|| graphql::pages(None, max_pages, |after, _| {
                let doc = Document { query: "query Page($after: String) { viewer { repositories(first: 1, after: $after) { pageInfo { endCursor } } } }".into(), variables: BTreeMap::from([("after".into(), serde_json::json!(after))]) };
                api.graphql("github.com", &doc, &RequestOptions::default())?.json::<serde_json::Value>()
            }, |page| page.get("next").and_then(|value| value.as_str()).map(str::to_owned), |_| false));
            let mut expected = None;
            for cursor in cursors {
                let exchange = fixture.next();
                let sent: serde_json::Value = serde_json::from_slice(&exchange.body).unwrap();
                assert_eq!(sent["variables"]["after"], serde_json::json!(expected));
                exchange.reply(
                    200,
                    "",
                    serde_json::json!({"data":{}, "next":cursor})
                        .to_string()
                        .as_bytes(),
                );
                expected = cursor;
            }
            assert_eq!(job.join().unwrap().unwrap().truncated, truncated);
        });
    }
}

#[cfg(unix)]
fn gh_fixture(store: &Store) -> Vec<(String, String)> {
    use std::os::unix::fs::PermissionsExt as _;
    let program = store.root.join("gh");
    std::fs::write(
        &program,
        r#"#!/bin/sh
[ "$GH_PROMPT_DISABLED" = 1 ] || exit 9
[ -z "$GH_DEBUG$GH_TOKEN$GITHUB_TOKEN$GH_ENTERPRISE_TOKEN$GITHUB_ENTERPRISE_TOKEN" ] || exit 9
printf '%s\n' "$*" >> "$FIXTURE_ROOT/calls"
if [ "$2" = status ]; then
  printf '%s' '{"hosts":{"github.com":[{"login":"first"},{"login":"second"}]}}'
  exit 0
fi
if [ "$6" = missing ]; then exit 1; fi
if [ -f "$FIXTURE_ROOT/signed-out" ]; then exit 1; fi
if [ -n "$6" ]; then printf '%s' "account-$6"; else /bin/cat "$FIXTURE_ROOT/token"; fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(store.root.join("token"), "cli-first").unwrap();
    vec![
        ("PATH".into(), store.root.to_string_lossy().into_owned()),
        (
            "FIXTURE_ROOT".into(),
            store.root.to_string_lossy().into_owned(),
        ),
    ]
}

#[cfg(unix)]
#[test]
fn gh_process_boundary_honors_binding_pins_cache_invalidation_and_failure_ttls() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let store = Store::new();
    let mut env = gh_fixture(&store);
    env.extend([
        ("GH_ENTERPRISE_TOKEN".into(), "enterprise-secret".into()),
        ("GH_HOST".into(), "allowed.example.com".into()),
    ]);
    let credentials = Credentials::new(store.store.clone(), env);
    let api = GitHubApi::new(credentials.clone(), fixture.builder());
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "untrusted.example.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer cli-first"));
    assert!(!request.contains("enterprise-secret"));
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "allowed.example.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer enterprise-secret"));
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    std::fs::write(store.root.join("token"), "cli-next").unwrap();
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        401,
        "",
        b"{}",
    );
    assert_eq!(answer.unwrap_err(), GitHubError::Unauthorized);
    assert!(request.contains("Bearer cli-first"));
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer cli-next"));
    for (account, token) in [
        ("first", "account-first"),
        ("second", "account-second"),
        ("missing", "cli-next"),
    ] {
        credentials.configure(BTreeMap::from([(
            "github.com".into(),
            GitHubHostSettings {
                enabled: true,
                account: Some(account.into()),
            },
        )]));
        let (answer, request, _) = fixture.call(
            || {
                api.rest(
                    "github.com",
                    RestRequest::get("/user"),
                    &RequestOptions::default(),
                )
            },
            200,
            "",
            b"{}",
        );
        answer.unwrap();
        assert!(request.contains(&format!("Bearer {token}")));
    }
    let mut env = gh_fixture(&store);
    env.extend([
        ("GH_TOKEN".into(), " ".into()),
        ("GITHUB_TOKEN".into(), "environment-fallback".into()),
    ]);
    let env_credentials = Credentials::new(store.store.clone(), env);
    env_credentials.configure(BTreeMap::from([(
        "github.com".into(),
        GitHubHostSettings {
            enabled: true,
            account: Some("second".into()),
        },
    )]));
    let env_api = GitHubApi::new(env_credentials.clone(), fixture.builder());
    let (answer, request, _) = fixture.call(
        || {
            env_api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.contains("Bearer environment-fallback"));
    let status = env_credentials.discover();
    assert!(status["github.com"].env_overrides_account);
    assert_eq!(status["github.com"].accounts, ["first", "second"]);
    assert_eq!(
        status["github.com"].source,
        Some(GitHubCredentialSource::Env)
    );
    // A negative CLI lookup is cached; a repaired executable failure is retried immediately.
    let retry_credentials = Credentials::new(store.store.clone(), gh_fixture(&store));
    let retry_api = GitHubApi::new(retry_credentials.clone(), fixture.builder());
    std::fs::set_permissions(
        store.root.join("gh"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        retry_api
            .rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default()
            )
            .unwrap_err(),
        GitHubError::Credential(CredentialError::CliFailed)
    );
    std::fs::set_permissions(
        store.root.join("gh"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let (answer, _, _) = fixture.call(
        || {
            retry_api.rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    retry_credentials.invalidate("github.com");
    std::fs::write(store.root.join("signed-out"), "").unwrap();
    assert_eq!(
        retry_api
            .rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default()
            )
            .unwrap_err(),
        GitHubError::Credential(CredentialError::NotSignedIn)
    );
    std::fs::remove_file(store.root.join("signed-out")).unwrap();
    assert_eq!(
        retry_api
            .rest(
                "github.com",
                RestRequest::get("/user"),
                &RequestOptions::default()
            )
            .unwrap_err(),
        GitHubError::Credential(CredentialError::NotSignedIn)
    );
    assert!(
        std::fs::read_to_string(store.root.join("calls"))
            .unwrap()
            .contains("auth token --hostname github.com --user second")
    );
}

#[test]
#[allow(clippy::disallowed_methods)] // Drive the same blocking-job cancellation used by HostCx::unblock.
fn dropped_waiters_keep_all_eight_socket_permits_until_their_jobs_finish() {
    let fixture = Fixture::new();
    let store = Store::new();
    let api = GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture")]),
        fixture.builder(),
    );
    let mut tasks = Vec::new();
    let mut exchanges = Vec::new();
    for _ in 0..8 {
        let api = api.clone();
        tasks.push(smol::spawn(async move {
            smol::unblock(move || {
                api.rest(
                    "github.com",
                    RestRequest::get("/held"),
                    &RequestOptions::default(),
                )
            })
            .await
        }));
        exchanges.push(fixture.next());
    }
    // Dropping every async owner leaves eight real sockets held by blocking jobs.
    drop(tasks);
    let (started_tx, started_rx) = mpsc::channel();
    let next_api = api.clone();
    let next = thread::spawn(move || {
        started_tx.send(()).unwrap();
        next_api.rest(
            "github.com",
            RestRequest::get("/ninth"),
            &RequestOptions::default(),
        )
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        fixture.incoming.recv_timeout(Duration::from_millis(200)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    exchanges.pop().unwrap().reply(200, "", b"{}");
    let ninth = fixture.next();
    assert!(ninth.request.starts_with("GET /ninth "));
    ninth.reply(200, "", b"{}");
    next.join().unwrap().unwrap();
    for exchange in exchanges {
        exchange.reply(200, "", b"{}");
    }
    // A round-trip after releasing all sockets ensures no abandoned I/O or fixture threads.
    let (answer, _, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/finished"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
}

#[test]
fn whole_response_result_deadline_includes_a_stalled_dns_resolver() {
    let fixture = Fixture::new();
    let store = Store::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    let address = fixture.address;
    let builder = fixture.builder().resolver(move |_: &str| {
        if first.swap(false, Ordering::SeqCst) {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        Ok(vec![address])
    });
    let api = GitHubApi::new(store.credentials(&[("GH_TOKEN", "fixture")]), builder);
    thread::scope(|scope| {
        let job = scope.spawn(|| {
            api.rest(
                "github.com",
                RestRequest::get("/dns"),
                &RequestOptions {
                    timeout: Duration::from_millis(200),
                    ..Default::default()
                },
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(job.join().unwrap().unwrap_err(), GitHubError::Deadline);
        release_tx.send(()).unwrap();
    });
    let (answer, request, _) = fixture.call(
        || {
            api.rest(
                "github.com",
                RestRequest::get("/after-dns"),
                &RequestOptions::default(),
            )
        },
        200,
        "",
        b"{}",
    );
    answer.unwrap();
    assert!(request.starts_with("GET /after-dns "));
}
