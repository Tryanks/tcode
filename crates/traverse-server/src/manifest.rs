//! `GET /relays.json`: this instance and its configured peers, in the shape
//! the client parses. `Manifest` and `ManifestRelay` duplicate the serde
//! structs in `crates/traverse/src/manifest.rs`; the server does not depend
//! on the client crate, so keep the two in step.
use std::net::SocketAddr;

use serde::Serialize;
use url::Url;

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub relays: Vec<ManifestRelay>,
    pub pkarr: Vec<Url>,
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestRelay {
    pub url: Url,
    /// `0` marks a relay without QUIC address discovery; absent means the
    /// client's default port, so a relay-only instance must send `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quic_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_rtt_max_ms: Option<u32>,
}

/// Where clients reach this instance: `https://<hostname>[:port]/` when a
/// hostname is configured (a TLS-terminating proxy in front of `tls.mode =
/// "off"` still serves HTTPS), else the plain listener itself.
pub fn public_base(config: &Config, http_addr: SocketAddr) -> Url {
    match config.hostname.as_deref().filter(|host| !host.is_empty()) {
        Some(hostname) => {
            let port = if config.tls_enabled() {
                config.tls.bind.port()
            } else {
                443
            };
            let authority = if port == 443 {
                hostname.to_string()
            } else {
                format!("{hostname}:{port}")
            };
            Url::parse(&format!("https://{authority}/")).expect("hostname forms a URL")
        }
        None => Url::parse(&format!("http://{http_addr}/")).expect("socket address forms a URL"),
    }
}

pub fn compose(config: &Config, base: &Url, updated_at: String) -> Manifest {
    let quic_port = if config.quic_enabled() {
        config.relay.quic_bind.port()
    } else {
        0
    };
    let mut relays = vec![ManifestRelay {
        url: base.clone(),
        quic_port: Some(quic_port),
        region: config.region.clone(),
        home_rtt_max_ms: config.lock.enabled.then_some(config.lock.home_rtt_max_ms),
    }];
    relays.extend(config.peers.iter().map(|peer| ManifestRelay {
        url: peer.url.clone(),
        quic_port: peer.quic_port,
        region: peer.region.clone(),
        home_rtt_max_ms: peer.home_rtt_max_ms,
    }));
    Manifest {
        version: 1,
        updated_at,
        relays,
        pkarr: vec![base.join("pkarr").expect("relative path")],
        dns: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Peer, TlsMode};

    fn json(manifest: &Manifest) -> serde_json::Value {
        serde_json::to_value(manifest).unwrap()
    }

    #[test]
    fn a_tls_instance_advertises_itself_qad_and_peers() {
        let mut config = Config::parse("hostname = \"traverse.example.org\"\ncontact = \"admin@example.org\"\nregion = \"eu\"\n[lock]\nenabled = true\nhome_rtt_max_ms = 60").unwrap();
        config.peers.push(Peer {
            url: Url::parse("https://relay-2.example.org/").unwrap(),
            quic_port: Some(7842),
            region: Some("us".into()),
            home_rtt_max_ms: None,
        });
        let base = public_base(&config, "127.0.0.1:1".parse().unwrap());
        let manifest = compose(&config, &base, "2026-09-21T00:00:00Z".into());
        assert_eq!(
            json(&manifest),
            serde_json::json!({
                "version": 1,
                "updatedAt": "2026-09-21T00:00:00Z",
                "relays": [
                    { "url": "https://traverse.example.org/", "quic_port": 7842, "region": "eu", "home_rtt_max_ms": 60 },
                    { "url": "https://relay-2.example.org/", "quic_port": 7842, "region": "us" }
                ],
                "pkarr": ["https://traverse.example.org/pkarr"],
                "dns": []
            })
        );
    }

    #[test]
    fn without_tls_the_relay_is_relay_only_and_a_proxied_hostname_stays_https() {
        let mut config =
            Config::parse("hostname = \"traverse.example.org\"\n[tls]\nmode = \"off\"").unwrap();
        let base = public_base(&config, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(base.as_str(), "https://traverse.example.org/");
        let manifest = compose(&config, &base, "x".into());
        assert_eq!(json(&manifest)["relays"][0]["quic_port"], 0);
        assert!(
            json(&manifest)["relays"][0]
                .get("home_rtt_max_ms")
                .is_none()
        );

        config.hostname = None;
        let base = public_base(&config, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(base.as_str(), "http://127.0.0.1:8080/");
        assert_eq!(
            compose(&config, &base, "x".into()).pkarr,
            [Url::parse("http://127.0.0.1:8080/pkarr").unwrap()]
        );

        config.tls.mode = TlsMode::Manual;
        config.tls.bind = "[::]:8443".parse().unwrap();
        config.hostname = Some("h.example".into());
        let base = public_base(&config, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(base.as_str(), "https://h.example:8443/");
        assert_eq!(
            compose(&config, &base, "x".into()).relays[0].quic_port,
            Some(7842)
        );
    }
}
