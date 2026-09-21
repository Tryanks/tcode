//! Saved machines and pairing invitations shared by all clients.
//!
//! A machine is identified by its iroh `EndpointId`; every other field is a
//! routing hint that discovery services may extend but never replace.
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use url::Url;

/// A pairing is bound to the machine identity (`host_id`), never to an
/// address. The hints are what the invite carried, refreshed after each
/// authenticated connection so the next launch starts from what worked last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedHost {
    /// The machine's `EndpointId`.
    pub host_id: String,
    pub name: String,
    /// Base URL of the Traverse instance the machine publishes to: `None`
    /// for the official service, [`TRAVERSE_OFF`] for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traverse: Option<String>,
    /// The machine's home relay, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// Direct `ip:port` addresses the machine was last reachable at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_unix: Option<u64>,
}

/// Direct addresses kept per machine.
pub const MAX_ADDRS: usize = 16;
/// The `traverse` value of a machine that publishes to no service, so a
/// device tells it apart from one on the official service and loads no
/// manifest for it.
pub const TRAVERSE_OFF: &str = "off";

/// Record a host in the saved list. A host is identified by `host_id`, so
/// pairing again or stamping a reconnection replaces its record instead of
/// adding a second row; the newest record moves to the end.
pub fn remember_host(hosts: &mut Vec<PairedHost>, host: PairedHost) {
    hosts.retain(|existing| existing.host_id != host.host_id);
    hosts.push(host);
}

/// What a `tcode://pair` link carries: the machine identity, where to reach
/// it, and the single-use secret that admits the device. First contact is
/// always by scanning or pasting the link, so the link itself is the secret;
/// there is no separate code. A browser leaves `host_id` empty and pairs with
/// the origin that served it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairInvite {
    pub host_id: String,
    pub name: String,
    /// [`SECRET_BYTES`] random bytes as unpadded base64url.
    pub secret: String,
    /// See [`PairedHost::traverse`].
    pub traverse: Option<String>,
    pub relay: Option<String>,
    pub addrs: Vec<String>,
}

impl PairInvite {
    /// The saved record a completed pairing produces, named as the machine
    /// introduced itself.
    pub fn paired(&self, host_name: String) -> PairedHost {
        PairedHost {
            host_id: self.host_id.clone(),
            name: host_name,
            traverse: self.traverse.clone(),
            relay: self.relay.clone(),
            addrs: self.addrs.clone(),
            last_connected_unix: None,
        }
    }
}

/// An `EndpointId` as printed by iroh: 64 hex characters.
pub fn valid_host_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Entropy of an invitation secret.
pub const SECRET_BYTES: usize = 16;
/// Length of an encoded secret: 16 bytes as unpadded base64url.
pub const SECRET_LEN: usize = 22;

/// Encode random bytes as the secret a link carries.
pub fn encode_secret(bytes: &[u8; SECRET_BYTES]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Whether `secret` is the canonical encoding of [`SECRET_BYTES`] bytes.
pub fn valid_invitation_secret(secret: &str) -> bool {
    secret.len() == SECRET_LEN
        && URL_SAFE_NO_PAD
            .decode(secret)
            .is_ok_and(|bytes| bytes.len() == SECRET_BYTES)
}

fn valid_addr(addr: &str) -> bool {
    addr.parse::<std::net::SocketAddr>()
        .is_ok_and(|addr| addr.port() != 0)
}

fn valid_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

pub fn parse_pair_url(value: &str) -> Option<PairInvite> {
    if value.len() > 4096 {
        return None;
    }
    let url = Url::parse(value.trim()).ok()?;
    if url.scheme() != "tcode" || url.host_str() != Some("pair") {
        return None;
    }
    let field = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if field("v")? != "2" {
        return None;
    }
    let host_id = field("id")?;
    if !valid_host_id(&host_id) {
        return None;
    }
    let secret = field("secret")?;
    if !valid_invitation_secret(&secret) {
        return None;
    }
    let traverse = field("traverse");
    let relay = field("relay");
    if traverse
        .as_deref()
        .is_some_and(|url| url != TRAVERSE_OFF && !valid_url(url))
        || relay.as_deref().is_some_and(|url| !valid_url(url))
    {
        return None;
    }
    let mut addrs: Vec<String> = Vec::new();
    for (_, addr) in url.query_pairs().filter(|(key, _)| key == "addr") {
        if !valid_addr(&addr) {
            return None;
        }
        if !addrs.iter().any(|known| *known == addr) {
            addrs.push(addr.into_owned());
        }
        if addrs.len() > MAX_ADDRS {
            return None;
        }
    }
    Some(PairInvite {
        host_id,
        name: field("name")?,
        secret,
        traverse,
        relay,
        addrs,
    })
}

pub fn pair_url(invite: &PairInvite) -> String {
    let mut url = Url::parse("tcode://pair").expect("static pairing URL is valid");
    let mut query = url.query_pairs_mut();
    query
        .append_pair("v", "2")
        .append_pair("id", &invite.host_id)
        .append_pair("secret", &invite.secret)
        .append_pair("name", &invite.name);
    if let Some(traverse) = &invite.traverse {
        query.append_pair("traverse", traverse);
    }
    if let Some(relay) = &invite.relay {
        query.append_pair("relay", relay);
    }
    for addr in invite.addrs.iter().take(MAX_ADDRS) {
        query.append_pair("addr", addr);
    }
    drop(query);
    url.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "a5f3c6ea6ba6c5c5d0c4e2b7a5f3c6ea6ba6c5c5d0c4e2b7a5f3c6ea6ba6c5c5";

    #[test]
    fn pairing_again_replaces_the_saved_host_instead_of_duplicating_it() {
        let host = |id: &str, name: &str| PairedHost {
            host_id: id.into(),
            name: name.into(),
            traverse: None,
            relay: None,
            addrs: vec!["192.168.1.2:47420".into()],
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
    fn saved_hosts_persist_identity_and_hints_only() {
        let host: PairedHost = serde_json::from_str(&format!(
            r#"{{"host_id":"{ID}","name":"Desk","relay":"https://euw1-1.relay.iroh.network./","addrs":["192.168.1.2:47420"],"last_connected_unix":42}}"#
        ))
        .unwrap();
        assert_eq!(host.traverse, None);
        assert_eq!(
            host.relay.as_deref(),
            Some("https://euw1-1.relay.iroh.network./")
        );
        assert_eq!(
            serde_json::to_value(&host).unwrap(),
            serde_json::json!({"host_id":ID,"name":"Desk","relay":"https://euw1-1.relay.iroh.network./","addrs":["192.168.1.2:47420"],"last_connected_unix":42})
        );
        let minimal: PairedHost =
            serde_json::from_str(&format!(r#"{{"host_id":"{ID}","name":"Desk"}}"#)).unwrap();
        assert!(minimal.addrs.is_empty());
        assert_eq!(minimal.last_connected_unix, None);
    }

    const SECRET: &str = "AAECAwQFBgcICQoLDA0ODw";

    #[test]
    fn secrets_are_sixteen_bytes_as_unpadded_base64url() {
        assert_eq!(encode_secret(&std::array::from_fn(|i| i as u8)), SECRET);
        assert!(valid_invitation_secret(SECRET));
        assert!(valid_invitation_secret("__-_AAAAAAAAAAAAAAAAAA"));
        for bad in [
            "",
            "123456",
            "AAECAwQFBgcICQoLDA0OD",    // 15 bytes and a half
            "AAECAwQFBgcICQoLDA0ODw==", // padded
            "AAECAwQFBgcICQoLDA0ODx",   // non-canonical trailing bits
            "AAECAwQFBgcICQoLDA0OD+",   // standard alphabet
            "AAECAwQFBgcICQoLDA0ODwAA", // 18 bytes
        ] {
            assert!(!valid_invitation_secret(bad), "{bad:?}");
        }
    }

    #[test]
    fn invitations_round_trip_and_reject_malformed_fields() {
        let wire = format!(
            "tcode://pair?v=2&id={ID}&secret={SECRET}&name=Desk&traverse=https%3A%2F%2Ftraverse.example%2F&relay=https%3A%2F%2Frelay.example%2F&addr=192.168.1.2%3A47420&addr=%5Bfd00%3A%3A2%5D%3A47420"
        );
        let invite = parse_pair_url(&wire).unwrap();
        assert_eq!(
            invite,
            PairInvite {
                host_id: ID.into(),
                name: "Desk".into(),
                secret: SECRET.into(),
                traverse: Some("https://traverse.example/".into()),
                relay: Some("https://relay.example/".into()),
                addrs: vec!["192.168.1.2:47420".into(), "[fd00::2]:47420".into()],
            }
        );
        assert_eq!(pair_url(&invite), wire);
        let lan_only = parse_pair_url(&format!(
            "tcode://pair?v=2&id={ID}&secret={SECRET}&name=Desk&addr=10.0.0.4%3A5000&addr=10.0.0.4%3A5000"
        ))
        .unwrap();
        assert_eq!(lan_only.addrs, ["10.0.0.4:5000"]);
        assert_eq!(lan_only.relay, None);
        let off = parse_pair_url(&format!(
            "tcode://pair?v=2&id={ID}&secret={SECRET}&name=Desk&traverse=off&addr=10.0.0.4%3A5000"
        ))
        .unwrap();
        assert_eq!(off.traverse.as_deref(), Some(TRAVERSE_OFF));
        assert!(pair_url(&off).contains("&traverse=off&"));
        for (field, replacement) in [
            ("v=2", "v=1"),
            ("tcode://", "https://"),
            // A link from before the secret carried a six-digit code.
            (&format!("secret={SECRET}"), "code=123456"),
            (&format!("secret={SECRET}"), "secret=123456"),
            (
                &format!("secret={SECRET}"),
                "secret=AAECAwQFBgcICQoLDA0ODw%3D%3D",
            ),
            (&format!("id={ID}"), "id=desk"),
            ("addr=192.168.1.2%3A47420", "addr=192.168.1.2"),
            ("addr=192.168.1.2%3A47420", "addr=192.168.1.2%3A0"),
            (
                "relay=https%3A%2F%2Frelay.example%2F",
                "relay=ftp%3A%2F%2Frelay",
            ),
            (
                "traverse=https%3A%2F%2Ftraverse.example%2F",
                "traverse=disabled",
            ),
        ] {
            assert!(
                parse_pair_url(&wire.replace(field, replacement)).is_none(),
                "{replacement}"
            );
        }
        assert!(parse_pair_url(&format!("{wire}&padding={}", "x".repeat(4096))).is_none());
        let many: String = (0..MAX_ADDRS + 1)
            .map(|n| format!("&addr=10.0.0.{n}%3A1"))
            .collect();
        assert!(
            parse_pair_url(&format!(
                "tcode://pair?v=2&id={ID}&secret={SECRET}&name=Desk{many}"
            ))
            .is_none()
        );
    }
}
