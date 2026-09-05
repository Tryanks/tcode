//! Client-side pairing data shared by every tcode client (desktop, phone,
//! browser): the paired-host record and the `tcode://pair?...` invite link.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use url::Url;

/// One host this client has paired with, as persisted in `hosts.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedHost {
    pub host_id: String,
    pub name: String,
    pub addrs: Vec<String>,
    pub port: u16,
    pub token: String,
    #[serde(default)]
    pub fingerprint: String,
    /// Unix seconds of the last successful connection; absent until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_unix: Option<u64>,
}

/// The decoded content of a pairing QR code or link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairInvite {
    pub host_id: String,
    pub name: String,
    pub addrs: Vec<String>,
    pub port: u16,
    pub code: String,
    pub fp: String,
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
    let port = fields.get("port")?.parse().ok()?;
    let addrs = fields
        .get("addrs")?
        .split(',')
        .filter(|address| !address.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if addrs.is_empty() {
        return None;
    }
    let code = fields.get("code")?.clone();
    if !is_pairing_code(&code) {
        return None;
    }
    let fp = fields.get("fp")?.to_ascii_lowercase();
    if !valid_fingerprint(&fp) || port == 0 {
        return None;
    }
    Some(PairInvite {
        fp,
        host_id: fields.get("host")?.clone(),
        name: fields.get("name")?.clone(),
        addrs,
        port,
        code,
    })
}

pub fn pair_url(invite: &PairInvite) -> String {
    let mut url = Url::parse("tcode://pair").expect("static pairing URL is valid");
    url.query_pairs_mut()
        .append_pair("v", "1")
        .append_pair("host", &invite.host_id)
        .append_pair("name", &invite.name)
        .append_pair("addrs", &invite.addrs.join(","))
        .append_pair("port", &invite.port.to_string())
        .append_pair("code", &invite.code)
        .append_pair("fp", &invite.fp);
    url.into()
}

/// A pairing code is exactly six ASCII digits.
pub fn is_pairing_code(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit())
}

/// Canonical SHA-256 certificate fingerprint (32 bytes, lowercase hex).
pub fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// First eight 16-bit hex groups for human comparison; full digest is pinned.
pub fn display_fingerprint(value: &str) -> String {
    if !valid_fingerprint(value) {
        return String::new();
    }
    let (groups, _) = value.as_bytes().as_chunks::<4>();
    groups
        .iter()
        .take(8)
        .map(|group| std::str::from_utf8(group).expect("hex is ASCII"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_url_round_trip() {
        let invite = PairInvite {
            host_id: "host-id".into(),
            name: "Desk & Mac".into(),
            addrs: vec!["192.168.1.2".into(), "fd00::1".into()],
            port: 47_420,
            code: "123456".into(),
            fp: "ab".repeat(32),
        };
        assert_eq!(parse_pair_url(&pair_url(&invite)), Some(invite));
        assert!(parse_pair_url("https://example.com").is_none());
        assert!(!is_pairing_code("12345"));
    }

    #[test]
    fn paired_host_tolerates_legacy_records() {
        let legacy = r#"{"host_id":"h","name":"n","addrs":["1.2.3.4"],"port":1,"token":"t"}"#;
        let host: PairedHost = serde_json::from_str(legacy).unwrap();
        assert_eq!(host.last_connected_unix, None);
    }
}
