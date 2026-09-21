//! A Traverse instance describes itself with `<base>/relays.json`:
//!
//! ```json
//! { "v": 1,
//!   "relays": [ { "url": "https://relay.example/", "quic_port": 7842 } ],
//!   "pkarr":  [ "https://traverse.example/pkarr/" ] }
//! ```
//!
//! A relay without `quic_port` uses iroh's default QUIC address-discovery
//! port; `"quic_port": 0` marks a relay that only forwards (no UDP).
use std::{fs, io, path::Path};

use iroh::{RelayConfig, RelayMap, RelayUrl};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub v: u32,
    pub relays: Vec<ManifestRelay>,
    #[serde(default)]
    pub pkarr: Vec<Url>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRelay {
    pub url: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quic_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_rtt_max_ms: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ManifestError {
    Json(String),
    Version(u32),
    /// A relay or pkarr URL is not `https`, or the relay URL is malformed.
    Url(String),
    NoRelays,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "invalid manifest: {error}"),
            Self::Version(version) => write!(f, "unsupported manifest version {version}"),
            Self::Url(url) => write!(f, "manifest URL must be https: {url}"),
            Self::NoRelays => f.write_str("manifest lists no relays"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    pub fn parse(json: &str) -> Result<Self, ManifestError> {
        let manifest: Self =
            serde_json::from_str(json).map_err(|error| ManifestError::Json(error.to_string()))?;
        if manifest.v != 1 {
            return Err(ManifestError::Version(manifest.v));
        }
        if manifest.relays.is_empty() {
            return Err(ManifestError::NoRelays);
        }
        for url in manifest
            .relays
            .iter()
            .map(|relay| &relay.url)
            .chain(&manifest.pkarr)
        {
            if url.scheme() != "https" || url.host_str().is_none() {
                return Err(ManifestError::Url(url.to_string()));
            }
        }
        Ok(manifest)
    }

    /// The relay map iroh dials for `RelayMode::Custom`.
    pub fn relay_map(&self) -> Result<RelayMap, ManifestError> {
        self.relays
            .iter()
            .map(|relay| {
                // `RelayQuicConfig` is not re-exported by iroh; start from the
                // default QUIC configuration and adjust or drop it.
                let mut config = RelayConfig::from(RelayUrl::from(relay.url.clone()));
                match relay.quic_port {
                    Some(0) => config.quic = None,
                    Some(port) => {
                        if let Some(quic) = &mut config.quic {
                            quic.port = port;
                        }
                    }
                    None => {}
                }
                Ok(config)
            })
            .collect()
    }

    pub fn pkarr_urls(&self) -> &[Url] {
        &self.pkarr
    }
}

/// Where the last manifest for `base` is kept.
pub fn cache_path(data_dir: &Path, base: &Url) -> std::path::PathBuf {
    let mut name = String::from("traverse-manifest-");
    for byte in base.as_str().bytes() {
        if byte.is_ascii_alphanumeric() {
            name.push(byte as char);
        } else {
            name.push('_');
        }
    }
    name.push_str(".json");
    data_dir.join(name)
}

/// The manifest for the Traverse instance at `base`.
///
/// TODO(traverse): fetch `<base>/relays.json` over HTTPS and refresh the
/// cache. Until then a `file://` base is read directly and any other base is
/// served from the cache written next to the machine's data, so a fetch
/// failure can never silently turn a self-hosted machine into an official one.
pub fn load(data_dir: &Path, base: &Url) -> io::Result<Manifest> {
    let json = if base.scheme() == "file" {
        let path = base
            .to_file_path()
            .map_err(|()| io::Error::new(io::ErrorKind::InvalidInput, "invalid file URL"))?;
        fs::read_to_string(path)?
    } else {
        let path = cache_path(data_dir, base);
        fs::read_to_string(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "no cached Traverse manifest for {base} at {}: {error}",
                    path.display()
                ),
            )
        })?
    };
    Manifest::parse(&json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_becomes_relay_map_and_pkarr_urls() {
        let manifest = Manifest::parse(
            r#"{"v":1,"relays":[{"url":"https://relay.example/","quic_port":7842,"region":"eu"},{"url":"https://tcp-only.example/","quic_port":0},{"url":"https://default.example/"}],"pkarr":["https://traverse.example/pkarr/"]}"#,
        )
        .unwrap();
        let map = manifest.relay_map().unwrap();
        assert_eq!(map.len(), 3);
        let relay = |url: &str| map.get(&RelayUrl::from(Url::parse(url).unwrap())).unwrap();
        assert_eq!(
            relay("https://relay.example/").quic.as_ref().unwrap().port,
            7842
        );
        assert!(relay("https://tcp-only.example/").quic.is_none());
        assert_eq!(
            relay("https://default.example/")
                .quic
                .as_ref()
                .unwrap()
                .port,
            RelayConfig::from(RelayUrl::from(
                Url::parse("https://default.example/").unwrap()
            ))
            .quic
            .unwrap()
            .port
        );
        assert_eq!(
            manifest.pkarr_urls(),
            [Url::parse("https://traverse.example/pkarr/").unwrap()]
        );
    }

    #[test]
    fn malformed_manifests_are_rejected() {
        assert!(matches!(
            Manifest::parse("not json"),
            Err(ManifestError::Json(_))
        ));
        assert_eq!(
            Manifest::parse(r#"{"v":2,"relays":[{"url":"https://relay.example/"}]}"#),
            Err(ManifestError::Version(2))
        );
        assert_eq!(
            Manifest::parse(r#"{"v":1,"relays":[]}"#),
            Err(ManifestError::NoRelays)
        );
        assert_eq!(
            Manifest::parse(r#"{"v":1,"relays":[{"url":"http://relay.example/"}]}"#),
            Err(ManifestError::Url("http://relay.example/".into()))
        );
        assert_eq!(
            Manifest::parse(
                r#"{"v":1,"relays":[{"url":"https://relay.example/"}],"pkarr":["ftp://x/"]}"#
            ),
            Err(ManifestError::Url("ftp://x/".into()))
        );
        assert!(matches!(
            Manifest::parse(r#"{"v":1,"relays":[{"url":"relay"}]}"#),
            Err(ManifestError::Json(_))
        ));
    }

    #[test]
    fn a_file_base_is_read_directly_and_a_missing_cache_is_an_error() {
        let dir = std::env::temp_dir().join(format!("tcode-manifest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("relays.json");
        fs::write(
            &path,
            r#"{"v":1,"relays":[{"url":"https://relay.example/"}]}"#,
        )
        .unwrap();
        let base = Url::from_file_path(&path).unwrap();
        assert_eq!(load(&dir, &base).unwrap().relays.len(), 1);
        let remote = Url::parse("https://traverse.example/").unwrap();
        assert_eq!(
            load(&dir, &remote).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs::write(
            cache_path(&dir, &remote),
            r#"{"v":1,"relays":[{"url":"https://cached.example/"}]}"#,
        )
        .unwrap();
        assert_eq!(
            load(&dir, &remote).unwrap().relays[0].url.as_str(),
            "https://cached.example/"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
