//! Pure helpers for the connect screen: turning what a user types into a code
//! the host will recognise, and turning a host's refusal into a sentence that
//! tells them what to do about it.
//!
//! Kept free of any UI or platform type so it is testable without a browser —
//! the mistakes this guards against (a code that fails only because it was typed
//! in lower case; a refusal shown as "network error") are exactly the ones a
//! headless test can pin down.

use sync_protocol::RefuseReason;

/// How many characters the host mints. Mirrors `sync-host`'s `CODE_LEN`; a code
/// of any other length cannot be one this host issued.
pub const CODE_LEN: usize = 6;

/// Canonicalise a code as typed into the form the host issued.
///
/// A code is read off one screen and typed into another, so people add spaces
/// or dashes ("K7 M2 QX") and type in whatever case is convenient. Dropping the
/// separators and upper-casing makes those the same code rather than three
/// different failures — the host's alphabet is upper-case only, so a lower-case
/// letter is never a real character, just a typed one.
pub fn normalize_code(raw: &str) -> String {
    raw.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Whether a normalised code is worth sending.
///
/// Length only, deliberately not the alphabet: the exact set of characters is a
/// host detail (`sync-host/src/pairing.rs`), and mirroring it here would make
/// this crate reject valid codes the day the host widens it. A wrong-but-well-
/// formed code is the host's to refuse, and it does so with one reason on
/// purpose.
pub fn is_complete_code(code: &str) -> bool {
    code.chars().count() == CODE_LEN
}

/// Phrase a handshake refusal as an instruction, not a stack trace.
///
/// The one thing the user must learn from a refusal is whether to retype the
/// code or check the address — so none of these read as a transport error, and
/// the pairing case names the code explicitly.
pub fn refusal_message(reason: &RefuseReason) -> String {
    match reason {
        RefuseReason::PairingRejected => {
            "That code was wrong or has expired. Read the current code off the host and try again."
                .to_string()
        }
        RefuseReason::Unauthorized => {
            "The host no longer accepts this device's saved token. Pair again to get a new one."
                .to_string()
        }
        RefuseReason::UnsupportedVersion { host_min, host_max } => format!(
            "This app and that host do not share a protocol version (host speaks {host_min}–{host_max}). Update whichever is older."
        ),
        RefuseReason::Unavailable { detail } => {
            format!("The host is not accepting connections right now: {detail}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_and_separators_do_not_change_the_code() {
        // The bug this exists to prevent: the same code typed three ways
        // resolving to three different strings, one of which the host rejects.
        assert_eq!(normalize_code("k7m2qx"), "K7M2QX");
        assert_eq!(normalize_code("K7 M2 QX"), "K7M2QX");
        assert_eq!(normalize_code(" k7-m2-qx "), "K7M2QX");
    }

    #[test]
    fn completeness_is_measured_after_normalising() {
        assert!(is_complete_code(&normalize_code("k7-m2-qx")));
        assert!(!is_complete_code(&normalize_code("k7m2")));
        assert!(!is_complete_code(&normalize_code("k7m2qxx")));
    }

    #[test]
    fn a_wrong_code_reads_as_a_typo_not_a_network_fault() {
        let message = refusal_message(&RefuseReason::PairingRejected);
        assert!(message.to_lowercase().contains("code"));
        assert!(!mentions_transport(&message));
    }

    #[test]
    fn every_refusal_is_actionable_and_never_a_transport_error() {
        for reason in [
            RefuseReason::PairingRejected,
            RefuseReason::Unauthorized,
            RefuseReason::UnsupportedVersion {
                host_min: 2,
                host_max: 3,
            },
            RefuseReason::Unavailable {
                detail: "at capacity".into(),
            },
        ] {
            let message = refusal_message(&reason);
            assert!(!message.is_empty());
            assert!(
                !mentions_transport(&message),
                "a refusal must not read as a connection failure: {message}"
            );
        }
    }

    /// The version mismatch has to say which side is behind, not just "refused".
    #[test]
    fn a_version_mismatch_names_the_hosts_range() {
        let message = refusal_message(&RefuseReason::UnsupportedVersion {
            host_min: 4,
            host_max: 7,
        });
        assert!(message.contains('4') && message.contains('7'));
    }

    fn mentions_transport(message: &str) -> bool {
        let lower = message.to_lowercase();
        [
            "network",
            "socket",
            "websocket",
            "connection error",
            "transport",
        ]
        .iter()
        .any(|term| lower.contains(term))
    }
}
