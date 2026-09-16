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
    /// Ordered, deduplicated and bounded: origins that completed hello most
    /// recently first, then address hints reported by the machine or found on
    /// the LAN. Never contains `origin`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    pub token: String,
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

    /// Append address hints that are not yet known. Origins that completed
    /// hello keep their place ahead of hints. Returns whether any were new.
    pub fn add_candidates<'a>(&mut self, origins: impl IntoIterator<Item = &'a str>) -> bool {
        let mut added = false;
        for origin in origins {
            if self.candidates.len() >= MAX_CANDIDATE_ORIGINS {
                break;
            }
            if origin != self.origin && !self.candidates.iter().any(|known| known == origin) {
                self.candidates.push(origin.to_owned());
                added = true;
            }
        }
        added
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
            last_connected_unix: saved.last_connected_unix,
        };
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
    pub code: String,
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
    let code = fields.get("code")?.clone();
    if !is_pairing_code(&code) {
        return None;
    }
    Some(PairInvite {
        host_id: fields.get("host")?.clone(),
        name: fields.get("name")?.clone(),
        origin,
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
            vec!["http://192.168.1.10:47420", "http://172.20.10.1:47420"]
        );
        let many: Vec<String> = (0..40)
            .map(|n| format!("http://10.1.0.{n}:47420"))
            .collect();
        host.add_candidates(many.iter().map(String::as_str));
        assert_eq!(host.candidates.len(), MAX_CANDIDATE_ORIGINS);
        assert_eq!(host.candidates[0], "http://192.168.1.10:47420");
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
    fn invitations_carry_an_origin_and_code() {
        let invite = parse_pair_url(
            "tcode://pair?v=1&host=h&name=Desk&origin=https%3A%2F%2Ftunnel.example.com&code=123456",
        )
        .unwrap();
        assert_eq!(invite.origin, "https://tunnel.example.com");
        assert_eq!(parse_pair_url(&pair_url(&invite)), Some(invite));
        assert!(!is_pairing_code("12345"));
    }
}
