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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SavedHost")]
pub struct PairedHost {
    pub host_id: String,
    pub name: String,
    pub origin: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_unix: Option<u64>,
}

#[derive(Deserialize)]
struct SavedHost {
    host_id: String,
    name: String,
    origin: Option<String>,
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
        Ok(Self {
            host_id: saved.host_id,
            name: saved.name,
            origin,
            token: saved.token,
            last_connected_unix: saved.last_connected_unix,
        })
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
    fn legacy_saved_hosts_migrate_without_retaining_pins() {
        let host: PairedHost = serde_json::from_str(r#"{"host_id":"h","name":"n","addrs":["fd00::1","192.168.1.2"],"port":47420,"token":"t","fingerprint":"old-pin","last_connected_unix":42}"#).unwrap();
        assert_eq!(host.origin, "http://[fd00::1]:47420");
        assert_eq!(
            serde_json::to_value(host).unwrap(),
            serde_json::json!({"host_id":"h","name":"n","origin":"http://[fd00::1]:47420","token":"t","last_connected_unix":42})
        );
    }
    #[test]
    fn legacy_record_without_last_connection_time_is_readable() {
        let host: PairedHost = serde_json::from_str(
            r#"{"host_id":"h","name":"n","addrs":["192.168.1.10"],"port":47420,"token":"t"}"#,
        )
        .unwrap();
        assert_eq!(host.origin, "http://192.168.1.10:47420");
        assert_eq!(host.last_connected_unix, None);
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
