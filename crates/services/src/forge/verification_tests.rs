use crate::{forge::Forge, settings::SettingsStore};
use std::{collections::BTreeMap, sync::Arc};
use tcode_core::{
    pull_request::{HostKind, PullRequestKey},
    settings::{CredentialSource, HostAccount, HostProblem, HostSettings, TokenCheck},
};
use tcode_protocol::PullRequestRead;
#[path = "../../tests/support/http.rs"]
pub(crate) mod http;
use http::Fixture;

pub(crate) fn verification_survives_restart(
    kind: HostKind,
    authority: &str,
    repository: &str,
    make: impl Fn(SettingsStore, ureq::Agent) -> Arc<dyn Forge>,
) {
    let root = std::env::temp_dir().join(format!("tcode-verification-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture::new();
    let store = SettingsStore::new(root.clone());
    let host = || {
        let host = make(SettingsStore::new(root.clone()), fixture.builder().build());
        host.configure(BTreeMap::from([(
            authority.to_owned(),
            HostSettings::new(kind),
        )]));
        host
    };
    let key = PullRequestKey {
        host: authority.into(),
        repository: repository.into(),
        number: 1,
    };
    let read = |host: &dyn Forge, path: &str| {
        host.read(
            &key,
            PullRequestRead::FileText {
                revision: "abcdef".into(),
                path: path.into(),
            },
        )
    };
    store
        .set_token(kind, authority, Some("saved-first"))
        .unwrap();
    let first = host();
    assert_eq!(
        first.credential_status()[authority].problem,
        Some(HostProblem::Unverified)
    );
    fixture
        .call(|| read(first.as_ref(), "first.txt"), 200, "", b"file")
        .0
        .unwrap();
    assert_eq!(first.credential_status()[authority].problem, None);
    drop(first);
    let restarted = host();
    assert_eq!(
        restarted.credential_status()[authority].problem,
        None,
        "verification survives a fresh host"
    );
    fixture
        .call(|| read(restarted.as_ref(), "rejected.txt"), 401, "", b"{}")
        .0
        .unwrap_err();
    assert!(
        matches!(
            host().credential_status()[authority].problem,
            Some(HostProblem::Rejected { status: 401, .. })
        ),
        "a refusal replaces persisted verification and survives a fresh host"
    );
    store
        .set_token(kind, authority, Some("saved-second"))
        .unwrap();
    assert_eq!(
        host().credential_status()[authority].problem,
        Some(HostProblem::Unverified)
    );
    // A response for the old credential must not verify the replacement saved while it ran.
    let current = host();
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| read(current.as_ref(), "pending.txt"));
        let exchange = fixture.next();
        store
            .set_token(kind, authority, Some("saved-third"))
            .unwrap();
        exchange.reply(200, "", b"file");
        pending.join().unwrap().unwrap();
    });
    assert_eq!(
        host().credential_status()[authority].problem,
        Some(HostProblem::Unverified)
    );
    let third = host();
    fixture
        .call(|| read(third.as_ref(), "third.txt"), 200, "", b"file")
        .0
        .unwrap();
    assert_eq!(host().credential_status()[authority].problem, None);
    store.set_token(kind, authority, None).unwrap();
    store
        .set_token(kind, authority, Some("saved-third"))
        .unwrap();
    assert_eq!(
        host().credential_status()[authority].problem,
        Some(HostProblem::Unverified),
        "clearing and restoring a token does not resurrect verification"
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// A kind's current-user endpoint as the host answers it, and the environment that names a
/// token for the authority.
pub(crate) struct CurrentUser {
    /// The request line's path, query excluded.
    pub(crate) path: &'static str,
    pub(crate) body: &'static [u8],
    pub(crate) account: HostAccount,
    /// The kind's further reads after the account, answered with these bodies.
    pub(crate) then: &'static [&'static [u8]],
    pub(crate) environment: &'static [(&'static str, &'static str)],
    /// Sent with a token where the kind takes an email.
    pub(crate) email: Option<&'static str>,
}

/// Asking whose a token is never saves it and reads its answer as accepted, refused or
/// unreachable; the account of a saved or environment token is learned for the status.
pub(crate) fn token_checks(
    kind: HostKind,
    authority: &str,
    user: CurrentUser,
    make: impl Fn(SettingsStore, ureq::AgentBuilder, Vec<(String, String)>) -> Arc<dyn Forge>,
) {
    let root = std::env::temp_dir().join(format!("tcode-token-checks-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture::new();
    let store = SettingsStore::new(root.clone());
    let host = |environment: Vec<(String, String)>| {
        let host = make(
            SettingsStore::new(root.clone()),
            fixture.builder(),
            environment,
        );
        host.configure(BTreeMap::from([(
            authority.to_owned(),
            HostSettings::new(kind),
        )]));
        host
    };
    // Every request the run sends is answered in order; the last answer is `status`.
    let answer = |run: &(dyn Fn() -> TokenCheck + Sync), status: u16, body: &[u8]| {
        std::thread::scope(|scope| {
            let job = scope.spawn(run);
            let first = fixture.next();
            let request = first.request.clone();
            first.reply(status, "", body);
            if status == 200 {
                for then in user.then {
                    fixture.next().reply(200, "", then);
                }
            }
            (job.join().unwrap(), request)
        })
    };
    let checking = host(Vec::new());
    let check = || checking.check_token(authority, "candidate-token", user.email);
    let (accepted, request) = answer(&check, 200, user.body);
    assert_eq!(
        accepted,
        TokenCheck::Accepted {
            account: Some(user.account.clone())
        }
    );
    assert!(
        request.starts_with(&format!("GET {}", user.path)),
        "{request}"
    );
    assert!(request.contains("candidate-token") || request.contains("Basic "));
    assert_eq!(
        store.token(kind, authority),
        None,
        "a checked token is not saved"
    );
    assert_eq!(
        answer(&check, 401, b"{}").0,
        TokenCheck::Rejected { status: 401 }
    );
    assert_eq!(
        answer(&check, 502, b"").0,
        TokenCheck::Unreachable { status: Some(502) }
    );
    let unanswered = std::thread::scope(|scope| {
        let job = scope.spawn(check);
        drop(fixture.next());
        job.join().unwrap()
    });
    assert_eq!(unanswered, TokenCheck::Unreachable { status: None });

    store
        .set_token(kind, authority, Some("saved-token"))
        .unwrap();
    let saved = host(Vec::new());
    let status = &saved.credential_status()[authority];
    assert_eq!(status.problem, Some(HostProblem::Unverified));
    assert_eq!(status.login, None);
    answer(
        &|| {
            saved.identify(authority);
            TokenCheck::Unconfirmed
        },
        200,
        user.body,
    );
    let status = &host(Vec::new()).credential_status()[authority];
    assert_eq!(status.problem, None, "the saved token's account is known");
    assert_eq!(status.login.as_deref(), Some(user.account.login.as_str()));

    store.set_token(kind, authority, None).unwrap();
    let environment: Vec<_> = user
        .environment
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let named = host(environment.clone());
    let status = &named.credential_status()[authority];
    assert!(matches!(status.source, Some(CredentialSource::Env { .. })));
    assert_eq!((status.problem.clone(), status.login.clone()), (None, None));
    answer(
        &|| {
            named.identify(authority);
            TokenCheck::Unconfirmed
        },
        401,
        b"{}",
    );
    assert!(
        matches!(
            host(environment).credential_status()[authority].problem,
            Some(HostProblem::Rejected { status: 401, .. })
        ),
        "a refused environment token is reported"
    );
    std::fs::remove_dir_all(root).unwrap();
}
