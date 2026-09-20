//! Saved machine origins and pairing invitations shared by all clients.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

pub const DEFAULT_REMOTE_PORT: u16 = 47_420;

/// Normalize an HTTP(S) origin. Shorthand addresses use the LAN default port;
/// explicit URLs retain their scheme's standard port.
pub fn parse_origin(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("invalid address".into());
    }
    let explicit = value.contains("://");
    let input = if explicit {
        value.to_owned()
    } else if value.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("http://[{value}]:{DEFAULT_REMOTE_PORT}")
    } else if !value.contains(':') || value.ends_with(']') {
        format!("http://{value}:{DEFAULT_REMOTE_PORT}")
    } else {
        format!("http://{value}")
    };
    let url = Url::parse(&input).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err("expected an http or https origin".into());
    }
    Ok(url.origin().ascii_serialization())
}

pub fn lan_origin(addr: &str, port: u16) -> String {
    if addr.contains(':') && !addr.starts_with('[') {
        format!("http://[{addr}]:{port}")
    } else {
        format!("http://{addr}:{port}")
    }
}

/// Alternate origins remembered per machine, beyond the one that last worked.
pub const MAX_CANDIDATE_ORIGINS: usize = 16;

/// A pairing is bound to the machine identity (`host_id`), never to an
/// address. `origin` is the last origin that completed hello; `candidates`
/// are other origins worth trying when it stops answering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SavedHost")]
pub struct PairedHost {
    pub host_id: String,
    pub name: String,
    pub origin: String,
    /// Deduplicated and bounded; newly learned hints replace the oldest hints.
    /// Promotion keeps the previous successful origin first among alternatives.
    /// Never contains `origin`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    pub token: String,
    /// Host signing key learned while pairing or through a token-authenticated
    /// migration. A network address or a discovered host id is not proof of identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_unix: Option<u64>,
}

impl PairedHost {
    /// Record that `origin` just completed hello. The previous origin becomes
    /// the first candidate. Returns whether the record changed.
    pub fn promote_origin(&mut self, origin: &str) -> bool {
        if self.origin == origin {
            return false;
        }
        let previous = std::mem::replace(&mut self.origin, origin.to_owned());
        self.candidates.retain(|candidate| candidate != origin);
        self.candidates.insert(0, previous);
        self.candidates.truncate(MAX_CANDIDATE_ORIGINS);
        true
    }

    /// New hints must remain usable even after the history fills up. Keep a
    /// bounded batch of new origins ahead of older hints; repeated observations
    /// do not reorder the list or interrupt an in-flight connection attempt.
    pub fn add_candidates<'a>(&mut self, origins: impl IntoIterator<Item = &'a str>) -> bool {
        let mut batch = Vec::new();
        for origin in origins {
            if batch.len() >= MAX_CANDIDATE_ORIGINS {
                break;
            }
            let Ok(origin) = parse_origin(origin) else {
                continue;
            };
            if origin != self.origin && !batch.contains(&origin) {
                batch.push(origin);
            }
        }
        // Bound the observed batch, not just its unseen portion: an oversized
        // repeated advertisement must not alternate between disjoint cache pages.
        if batch.iter().all(|origin| self.candidates.contains(origin)) {
            return false;
        }
        self.candidates.retain(|origin| !batch.contains(origin));
        batch.append(&mut self.candidates);
        batch.truncate(MAX_CANDIDATE_ORIGINS);
        self.candidates = batch;
        true
    }
}

/// Record a host in the saved list. A host is identified by `host_id`, so
/// pairing again or stamping a reconnection replaces its record instead of
/// adding a second row; the newest record moves to the end.
pub fn remember_host(hosts: &mut Vec<PairedHost>, host: PairedHost) {
    hosts.retain(|existing| existing.host_id != host.host_id);
    hosts.push(host);
}

#[derive(Deserialize)]
struct SavedHost {
    host_id: String,
    name: String,
    origin: Option<String>,
    #[serde(default)]
    candidates: Vec<String>,
    #[serde(default)]
    addrs: Vec<String>,
    port: Option<u16>,
    token: String,
    #[serde(default)]
    identity_key: Option<String>,
    last_connected_unix: Option<u64>,
}
impl TryFrom<SavedHost> for PairedHost {
    type Error = String;
    fn try_from(saved: SavedHost) -> Result<Self, String> {
        let origin = match saved.origin {
            Some(origin) => parse_origin(&origin)?,
            None => parse_origin(&lan_origin(
                saved.addrs.first().ok_or("missing address")?,
                saved.port.ok_or("missing port")?,
            ))?,
        };
        let mut host = Self {
            host_id: saved.host_id,
            name: saved.name,
            origin,
            candidates: Vec::new(),
            token: saved.token,
            identity_key: saved.identity_key.map(|key| key.to_ascii_lowercase()),
            last_connected_unix: saved.last_connected_unix,
        };
        if host
            .identity_key
            .as_deref()
            .is_some_and(|key| !valid_identity_key(key))
        {
            return Err("invalid machine identity key".into());
        }
        // Older records listed every LAN address; the ones after the first are
        // the same hints a current host reports in hello.
        let legacy = saved.port.map_or(Vec::new(), |port| {
            saved
                .addrs
                .iter()
                .map(|addr| lan_origin(addr, port))
                .collect()
        });
        let known: Vec<String> = saved
            .candidates
            .iter()
            .chain(&legacy)
            .filter_map(|candidate| parse_origin(candidate).ok())
            .collect();
        host.add_candidates(known.iter().map(String::as_str));
        Ok(host)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairInvite {
    pub host_id: String,
    pub name: String,
    pub origin: String,
    pub candidates: Vec<String>,
    pub identity_key: Option<String>,
    pub code: String,
}

pub fn valid_identity_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn parse_pair_url(value: &str) -> Option<PairInvite> {
    if value.len() > 4096 {
        return None;
    }
    let url = Url::parse(value.trim()).ok()?;
    if url.scheme() != "tcode" || url.host_str() != Some("pair") {
        return None;
    }
    let fields: HashMap<_, _> = url.query_pairs().into_owned().collect();
    if fields.get("v")? != "1" {
        return None;
    }
    let origin = parse_origin(fields.get("origin")?).ok()?;
    let identity_key = fields
        .get("identity_key")
        .map(|key| key.to_ascii_lowercase());
    if identity_key
        .as_deref()
        .is_some_and(|key| !valid_identity_key(key))
    {
        return None;
    }
    let mut candidates = Vec::new();
    for (_, value) in url.query_pairs().filter(|(key, _)| key == "candidate") {
        let candidate = parse_origin(&value).ok()?;
        // A secure invite never authorizes downgrading its code to HTTP.
        if origin.starts_with("https:") && !candidate.starts_with("https:") {
            return None;
        }
        if candidate != origin && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
        if candidates.len() > MAX_CANDIDATE_ORIGINS {
            return None;
        }
    }
    let code = fields.get("code")?.clone();
    if !is_pairing_code(&code) {
        return None;
    }
    Some(PairInvite {
        host_id: fields.get("host")?.clone(),
        name: fields.get("name")?.clone(),
        origin,
        candidates,
        identity_key,
        code,
    })
}
pub fn pair_url(invite: &PairInvite) -> String {
    let mut url = Url::parse("tcode://pair").expect("static pairing URL is valid");
    url.query_pairs_mut()
        .append_pair("v", "1")
        .append_pair("host", &invite.host_id)
        .append_pair("name", &invite.name)
        .append_pair("origin", &invite.origin)
        .append_pair("code", &invite.code);
    for candidate in invite.candidates.iter().take(MAX_CANDIDATE_ORIGINS) {
        url.query_pairs_mut().append_pair("candidate", candidate);
    }
    if let Some(key) = &invite.identity_key {
        url.query_pairs_mut().append_pair("identity_key", key);
    }
    url.into()
}
pub fn is_pairing_code(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origins_accept_lan_shorthand_and_tunnel_urls() {
        for (input, expected) in [
            ("desk", "http://desk:47420"),
            ("desk:1234", "http://desk:1234"),
            ("desk:80", "http://desk"),
            ("http://desk", "http://desk"),
            ("https://tunnel.example.com", "https://tunnel.example.com"),
            ("https://desk:8443/", "https://desk:8443"),
            ("::1", "http://[::1]:47420"),
            ("[fd00::1]:1234", "http://[fd00::1]:1234"),
        ] {
            assert_eq!(parse_origin(input).unwrap(), expected);
        }
        for input in [
            "",
            "ftp://desk",
            "https://user:password@desk",
            "http://desk/path",
            "desk:0",
        ] {
            assert!(parse_origin(input).is_err(), "{input}");
        }
    }
    #[test]
    fn older_hosts_json_records_load_and_keep_their_extra_addresses_as_candidates() {
        // Written by clients before origins existed: addresses plus port, and a
        // certificate pin that is no longer used.
        let host: PairedHost = serde_json::from_str(r#"{"host_id":"h","name":"n","addrs":["fd00::1","192.168.1.2"],"port":47420,"token":"t","fingerprint":"old-pin","last_connected_unix":42}"#).unwrap();
        assert_eq!(host.origin, "http://[fd00::1]:47420");
        assert_eq!(host.candidates, vec!["http://192.168.1.2:47420"]);
        assert_eq!(
            serde_json::to_value(&host).unwrap(),
            serde_json::json!({"host_id":"h","name":"n","origin":"http://[fd00::1]:47420","candidates":["http://192.168.1.2:47420"],"token":"t","last_connected_unix":42})
        );
        // Written by clients that saved one origin and no candidates.
        let host: PairedHost = serde_json::from_str(
            r#"{"host_id":"h","name":"n","origin":"http://192.168.1.10:47420","token":"t"}"#,
        )
        .unwrap();
        assert_eq!(host.origin, "http://192.168.1.10:47420");
        assert!(host.candidates.is_empty());
        assert_eq!(host.last_connected_unix, None);
        assert_eq!(
            serde_json::to_value(&host).unwrap(),
            serde_json::json!({"host_id":"h","name":"n","origin":"http://192.168.1.10:47420","token":"t"})
        );
        let host: PairedHost = serde_json::from_str(
            r#"{"host_id":"h","name":"n","addrs":["192.168.1.10"],"port":47420,"token":"t"}"#,
        )
        .unwrap();
        assert_eq!(host.origin, "http://192.168.1.10:47420");
        assert!(host.candidates.is_empty());
        // A candidate equal to the origin or malformed is dropped on load.
        let host: PairedHost = serde_json::from_str(
            r#"{"host_id":"h","name":"n","origin":"http://192.168.1.10:47420","candidates":["http://192.168.1.10:47420","not an origin","http://10.0.0.5:47420"],"token":"t"}"#,
        )
        .unwrap();
        assert_eq!(host.candidates, vec!["http://10.0.0.5:47420"]);
    }

    #[test]
    fn promoted_origins_lead_the_candidates_and_hints_stay_bounded() {
        let mut host = PairedHost {
            host_id: "h".into(),
            name: "n".into(),
            origin: "http://192.168.1.10:47420".into(),
            candidates: vec!["http://10.0.0.5:47420".into()],
            token: "t".into(),
            identity_key: None,
            last_connected_unix: None,
        };
        assert!(!host.promote_origin("http://192.168.1.10:47420"));
        assert!(host.promote_origin("http://10.0.0.5:47420"));
        assert_eq!(host.origin, "http://10.0.0.5:47420");
        assert_eq!(host.candidates, vec!["http://192.168.1.10:47420"]);
        assert!(host.add_candidates(["http://10.0.0.5:47420", "http://172.20.10.1:47420"]));
        assert!(!host.add_candidates(["http://172.20.10.1:47420"]));
        assert_eq!(
            host.candidates,
            vec!["http://172.20.10.1:47420", "http://192.168.1.10:47420"]
        );
        let many: Vec<String> = (0..40)
            .map(|n| format!("http://10.1.0.{n}:47420"))
            .collect();
        host.add_candidates(many.iter().map(String::as_str));
        assert_eq!(host.candidates.len(), MAX_CANDIDATE_ORIGINS);
        assert_eq!(host.candidates[0], "http://10.1.0.0:47420");
        assert!(
            !host.add_candidates(many.iter().map(String::as_str)),
            "an oversized repeated discovery result must not rotate the cache and restart recovery"
        );
        let office = "http://192.168.1.161:47420";
        assert!(
            host.add_candidates([office]),
            "a new network must replace stale hints"
        );
        assert_eq!(host.candidates[0], office);
        assert_eq!(host.candidates.len(), MAX_CANDIDATE_ORIGINS);
        assert!(!host.candidates.contains(&"http://10.1.0.15:47420".into()));
        assert!(
            !host.add_candidates([office]),
            "the same hint must not restart discovery"
        );
        assert!(host.promote_origin("http://10.1.0.13:47420"));
        assert_eq!(host.candidates.len(), MAX_CANDIDATE_ORIGINS);
        assert_eq!(host.candidates[0], "http://10.0.0.5:47420");
        assert!(
            !host
                .candidates
                .contains(&"http://10.1.0.13:47420".to_owned())
        );
    }

    #[test]
    fn pairing_again_replaces_the_saved_host_instead_of_duplicating_it() {
        let host = |id: &str, token: &str| PairedHost {
            host_id: id.into(),
            name: id.to_uppercase(),
            origin: "http://192.168.1.2:47420".into(),
            candidates: Vec::new(),
            token: token.into(),
            identity_key: None,
            last_connected_unix: None,
        };
        let mut hosts = vec![host("desk", "first"), host("laptop", "laptop")];
        remember_host(&mut hosts, host("desk", "second"));
        assert_eq!(
            hosts,
            vec![host("laptop", "laptop"), host("desk", "second")]
        );
    }

    #[test]
    fn invitations_preserve_identity_and_reject_invalid_or_downgraded_routes() {
        let wire =
            "tcode://pair?v=1&host=h&name=Desk&origin=https%3A%2F%2Ftunnel.example.com&code=123456";
        let invite = parse_pair_url(wire).unwrap();
        assert_eq!(
            invite,
            PairInvite {
                host_id: "h".into(),
                name: "Desk".into(),
                origin: "https://tunnel.example.com".into(),
                candidates: vec![],
                identity_key: None,
                code: "123456".into(),
            }
        );
        assert_eq!(pair_url(&invite), wire);
        for (field, replacement) in [
            ("v=1", "v=2"),
            ("tcode://", "https://"),
            ("code=123456", "code=12345"),
            ("code=123456", "code=1234567"),
            ("code=123456", "code=12x456"),
        ] {
            assert!(
                parse_pair_url(&wire.replace(field, replacement)).is_none(),
                "{replacement}"
            );
        }
        assert!(parse_pair_url(&format!("{wire}&padding={}", "x".repeat(4096))).is_none());
        let key = "11".repeat(32);
        let invite = parse_pair_url(&format!(
            "tcode://pair?v=1&host=desk&name=Desk&origin=http%3A%2F%2F192.168.31.42%3A47420&code=123456&candidate=http%3A%2F%2F192.168.139.3%3A47420&candidate=http%3A%2F%2F192.168.139.3%3A47420&identity_key={key}"
        )).unwrap();
        assert_eq!(invite.candidates, ["http://192.168.139.3:47420"]);
        assert_eq!(invite.identity_key.as_deref(), Some(key.as_str()));
        assert!(parse_pair_url("tcode://pair?v=1&host=desk&name=Desk&origin=https%3A%2F%2Fdesk.example&code=123456&candidate=http%3A%2F%2F192.168.1.2").is_none());
        assert!(parse_pair_url("tcode://pair?v=1&host=desk&name=Desk&origin=http%3A%2F%2F192.168.1.2&code=123456&identity_key=broken").is_none());
    }
}
