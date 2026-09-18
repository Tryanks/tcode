//! Authenticate a discovered endpoint before sending it a bearer token.
//!
//! A pinned Ed25519 key survives token revocation. Existing pairings bootstrap
//! the pin with an HMAC under their token hash, which the host already stores.
//! This avoids disclosing a bearer token to an unrelated LAN address; it does
//! not encrypt the subsequent stream or prevent an active plaintext relay.

use hmac::{Hmac, Mac as _};
use ring::signature;
#[cfg(feature = "server")]
use ring::signature::KeyPair as _;
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "client", test))]
use sha2::Digest as _;
use sha2::Sha256;

const DOMAIN: &[u8] = b"tcode-host-identity-v1\0";
pub(crate) const MAX_IDENTITY_MESSAGE_BYTES: usize = 4096;

#[derive(Serialize, Deserialize)]
pub(crate) struct IdentityChallenge {
    #[serde(rename = "type")]
    kind: String,
    pub(crate) host_id: String,
    pub(crate) token_id: String,
    nonce: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct IdentityProof {
    #[serde(rename = "type")]
    kind: String,
    host_id: String,
    nonce: String,
    identity_key: String,
    signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
}

impl IdentityChallenge {
    #[cfg(any(feature = "client", test))]
    pub(crate) fn new(host_id: &str, token: &str) -> std::io::Result<Self> {
        // A pinned host can still prove its identity when the saved token is
        // damaged or revoked, then authoritatively reject hello. Token shape
        // is not a prerequisite for authenticating the host.
        Self::with_token_id(
            host_id,
            encode_hex(&Sha256::digest(Sha256::digest(token.as_bytes()))),
        )
    }

    #[cfg(any(feature = "client", test))]
    pub(crate) fn for_pairing(host_id: &str) -> std::io::Result<Self> {
        let mut token_id = [0; 32];
        getrandom::fill(&mut token_id).map_err(std::io::Error::other)?;
        Self::with_token_id(host_id, encode_hex(&token_id))
    }

    #[cfg(any(feature = "client", test))]
    fn with_token_id(host_id: &str, token_id: String) -> std::io::Result<Self> {
        let mut nonce = [0; 32];
        getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
        let challenge = Self {
            kind: "identify".into(),
            host_id: host_id.into(),
            token_id,
            nonce: encode_hex(&nonce),
        };
        if !challenge.is_valid() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid saved host credentials",
            ));
        }
        Ok(challenge)
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.kind == "identify"
            && !self.host_id.is_empty()
            && self.host_id.len() <= 64
            && !self.host_id.chars().any(char::is_control)
            && decode_hex::<32>(&self.nonce).is_some()
            && decode_hex::<32>(&self.token_id).is_some()
    }

    fn message(&self, public_key: &[u8; 32]) -> Option<Vec<u8>> {
        if !self.is_valid() {
            return None;
        }
        let mut message = Vec::with_capacity(DOMAIN.len() + self.host_id.len() + 65);
        message.extend_from_slice(DOMAIN);
        message.extend_from_slice(self.host_id.as_bytes());
        message.push(0);
        message.extend_from_slice(&decode_hex::<32>(&self.nonce)?);
        message.extend_from_slice(public_key);
        Some(message)
    }

    #[cfg(feature = "server")]
    pub(crate) fn prove(
        &self,
        key: &signature::Ed25519KeyPair,
        token_hash: Option<&[u8; 32]>,
    ) -> Option<IdentityProof> {
        let public_key = key.public_key().as_ref().try_into().ok()?;
        let message = self.message(&public_key)?;
        let mac = token_hash.map(|hash| {
            let mut mac = Hmac::<Sha256>::new_from_slice(hash).expect("SHA256 hash is a valid key");
            mac.update(&message);
            encode_hex(&mac.finalize().into_bytes())
        });
        Some(IdentityProof {
            kind: "identity".into(),
            host_id: self.host_id.clone(),
            nonce: self.nonce.clone(),
            identity_key: encode_hex(&public_key),
            signature: encode_hex(key.sign(&message).as_ref()),
            mac,
        })
    }

    /// Returns only an authenticated public key. With no pin, the HMAC binds
    /// the key to the existing pairing before any bearer token is disclosed.
    #[cfg(any(feature = "client", test))]
    pub(crate) fn verify(
        &self,
        token: &str,
        pinned_key: Option<&str>,
        response: &serde_json::Value,
    ) -> Option<String> {
        let proof: IdentityProof = serde_json::from_value(response.clone()).ok()?;
        if proof.kind != "identity" || proof.host_id != self.host_id || proof.nonce != self.nonce {
            return None;
        }
        let public_key = decode_hex::<32>(&proof.identity_key)?;
        let message = self.message(&public_key)?;
        if let Some(pinned) = pinned_key {
            if decode_hex::<32>(pinned)? != public_key {
                return None;
            }
        } else {
            let supplied_mac = decode_hex::<32>(proof.mac.as_deref()?)?;
            let mut mac = Hmac::<Sha256>::new_from_slice(&Sha256::digest(token.as_bytes())).ok()?;
            mac.update(&message);
            mac.verify_slice(&supplied_mac).ok()?;
        }
        signature::UnparsedPublicKey::new(&signature::ED25519, public_key)
            .verify(&message, &decode_hex::<64>(&proof.signature)?)
            .ok()?;
        Some(encode_hex(&public_key))
    }
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 || !value.is_ascii() {
        return None;
    }
    let mut bytes = [0; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b";
    const PUBLIC_KEY: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn fixture() -> (IdentityChallenge, serde_json::Value) {
        let challenge = serde_json::from_value(serde_json::json!({
            "type": "identify", "host_id": HOST, "nonce": "12".repeat(32),
            "token_id": "b78ea9f06b76cc863fcb2714014a7f03a3fb523d9e74f5895bfe8c2cc8f89d3d",
        }))
        .unwrap();
        // RFC 8032 test key; signature generated independently with OpenSSL
        // pkeyutl -sign -rawin, MAC with Python hashlib/hmac. This fixes the v1
        // wire encoding, including the domain, host separator, and key binding.
        let proof = serde_json::json!({
            "type": "identity", "host_id": HOST, "nonce": "12".repeat(32),
            "identity_key": PUBLIC_KEY,
            "signature": "4b4068fefec5337d03c96752325257570966c146f3bc8392f47dfc83d0e3079545f395e77beb9210b6b4e30dfe43a1c66cbc1b30a1a1b6e91ee64ffd5c40f702",
            "mac": "46f16081f87640f916c88c7c01e6ccc7c61c086acf22dc6ded93230c104eed1e",
        });
        (challenge, proof)
    }

    #[test]
    fn existing_token_bootstraps_the_pinned_key_and_rejects_forgery_or_replay() {
        let (challenge, proof) = fixture();
        assert_eq!(
            challenge.verify(TOKEN, None, &proof).as_deref(),
            Some(PUBLIC_KEY)
        );
        assert_eq!(
            challenge.verify(TOKEN, Some(PUBLIC_KEY), &proof).as_deref(),
            Some(PUBLIC_KEY)
        );
        for (field, bad) in [
            ("type", "hello_ok".into()),
            ("host_id", "another-host".into()),
            ("nonce", "34".repeat(32)),
            ("identity_key", "56".repeat(32)),
            ("signature", "78".repeat(64)),
            ("mac", "9a".repeat(32)),
            ("mac", "a".repeat(65)),
        ] {
            let mut altered = proof.clone();
            altered[field] = bad.into();
            assert!(
                challenge.verify(TOKEN, None, &altered).is_none(),
                "accepted altered {field}"
            );
        }
        assert!(challenge.verify(&"B".repeat(43), None, &proof).is_none());
        assert!(
            challenge
                .verify(TOKEN, Some(&"ab".repeat(32)), &proof)
                .is_none()
        );
        let next_connection = IdentityChallenge::new(HOST, TOKEN).unwrap();
        assert!(
            next_connection
                .verify(TOKEN, Some(PUBLIC_KEY), &proof)
                .is_none()
        );
        let next_pairing = IdentityChallenge::for_pairing(HOST).unwrap();
        assert!(next_pairing.verify("", Some(PUBLIC_KEY), &proof).is_none());
    }

    #[test]
    fn a_pin_recognizes_the_host_after_token_revocation_without_unauthenticated_bootstrap() {
        let (challenge, mut proof) = fixture();
        proof.as_object_mut().unwrap().remove("mac");
        assert!(challenge.verify(TOKEN, None, &proof).is_none());
        assert_eq!(
            challenge.verify(TOKEN, Some(PUBLIC_KEY), &proof).as_deref(),
            Some(PUBLIC_KEY)
        );
    }

    #[cfg(feature = "server")]
    #[test]
    fn host_proof_matches_the_external_wire_fixture() {
        let (challenge, proof) = fixture();
        let seed =
            decode_hex::<32>("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
                .unwrap();
        let key = signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        let hash: [u8; 32] = Sha256::digest(TOKEN.as_bytes()).into();
        assert_eq!(
            serde_json::to_value(challenge.prove(&key, Some(&hash)).unwrap()).unwrap(),
            proof
        );
    }
}
