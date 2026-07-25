//! Short-lived pairing codes.
//!
//! A phone cannot be handed a token the way a query string hands one to a
//! browser, and reading a UUID off another screen is not a flow anyone will
//! use. So the host mints a short code, shows it, and trades it once for the
//! durable token.
//!
//! Three properties make a six-character code defensible where a six-character
//! password would not be:
//!
//! * it expires in minutes, so an attacker has a bounded window rather than
//!   forever;
//! * it is single-use, so a correct guess still races the real client;
//! * it is only worth anything to someone who can already reach the host's
//!   socket, which on the default loopback binding means local access.
//!
//! Widen the bind address and those assumptions change — which is why `--bind`
//! says so, and why this file is the place to add rate limiting when it does.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a code stays usable.
///
/// Long enough to walk to another device, short enough that a code left on a
/// screen is not a standing invitation.
pub const CODE_LIFETIME: Duration = Duration::from_secs(180);

/// Characters a code is drawn from.
///
/// Digits and uppercase letters minus the pairs people misread aloud or
/// mistype: no O/0, no I/1/L, no S/5, no B/8. A code exists to be read off one
/// screen and typed into another, so ambiguity is a usability bug that reads as
/// "pairing is broken".
const ALPHABET: &[u8] = b"ACDEFGHJKMNPQRTUVWXY23467";

/// Length of a generated code. 25^6 is about 244 million, which against a
/// three-minute single-use window is far more than a guesser gets to try.
const CODE_LEN: usize = 6;

#[derive(Clone)]
struct PendingCode {
    code: String,
    issued: Instant,
}

/// The host's pairing state: at most one outstanding code.
///
/// One, not a set: pairing is a deliberate act a person performs, and a host
/// holding several live codes is a host that cannot tell you which screen the
/// valid one is on.
#[derive(Clone, Default)]
pub struct Pairing {
    pending: Arc<Mutex<Option<PendingCode>>>,
}

impl Pairing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a code, replacing any outstanding one.
    ///
    /// Replacing rather than refusing: asking again is what a user does when
    /// the first code scrolled away, and making them wait out an expiry would
    /// be punishing them for the interface's shortcoming.
    /// `entropy` supplies the raw bytes a code is drawn from — at least
    /// [`CODE_LEN`] of them. Injected rather than sourced here so tests can be
    /// deterministic, and so the caller owns the choice of RNG.
    ///
    /// It must be *random*. An earlier version called the system clock once per
    /// character; on macOS the six reads landed in the same microsecond and
    /// every code came out `AAAAAA`. A code's security rests on being
    /// unguessable within its window, so a low-entropy source does not weaken
    /// it — it removes it.
    pub fn issue(&self, now: Instant, entropy: impl Fn() -> [u8; CODE_LEN]) -> String {
        let bytes = entropy();
        let code: String = bytes
            .iter()
            .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
            .collect();
        *self.lock() = Some(PendingCode {
            code: code.clone(),
            issued: now,
        });
        code
    }

    /// Consume `candidate` if it matches an unexpired code.
    ///
    /// Takes the code out of the slot whether or not it matched *when it
    /// matched*, so a correct code cannot be replayed. A wrong guess leaves the
    /// real code in place, so a wrong attempt cannot be used to cancel someone
    /// else's pairing.
    pub fn redeem(&self, candidate: &str, now: Instant) -> bool {
        let mut slot = self.lock();
        let Some(pending) = slot.as_ref() else {
            return false;
        };
        if now.duration_since(pending.issued) > CODE_LIFETIME {
            // Expired codes are cleared on contact rather than by a timer: there
            // is no background task to run, and a stale entry is only reachable
            // through this function anyway.
            *slot = None;
            return false;
        }
        if !constant_time_eq(candidate.as_bytes(), pending.code.as_bytes()) {
            return false;
        }
        *slot = None;
        true
    }

    /// The outstanding code, if one is live. For the host to display.
    pub fn current(&self, now: Instant) -> Option<String> {
        let slot = self.lock();
        slot.as_ref()
            .filter(|pending| now.duration_since(pending.issued) <= CODE_LIFETIME)
            .map(|pending| pending.code.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<PendingCode>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Compare without leaking the matching prefix length through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        .eq(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic bytes so a test can predict the code.
    fn fixed(seed: u8) -> impl Fn() -> [u8; CODE_LEN] {
        move || std::array::from_fn(|index| seed.wrapping_add(index as u8))
    }

    #[test]
    fn a_fresh_code_redeems_once() {
        let pairing = Pairing::new();
        let now = Instant::now();
        let code = pairing.issue(now, fixed(0));

        assert!(pairing.redeem(&code, now));
        // Single use is the property that makes a six-character code viable: a
        // correct guess still has to beat the real client to it.
        assert!(!pairing.redeem(&code, now), "a code must not redeem twice");
    }

    #[test]
    fn an_expired_code_is_refused() {
        let pairing = Pairing::new();
        let now = Instant::now();
        let code = pairing.issue(now, fixed(0));
        let later = now + CODE_LIFETIME + Duration::from_secs(1);

        assert!(!pairing.redeem(&code, later));
        assert_eq!(pairing.current(later), None, "expiry must also hide it");
    }

    /// A wrong guess must not clear the slot, or anyone able to reach the socket
    /// could cancel a pairing in progress by typing nonsense.
    #[test]
    fn a_wrong_guess_leaves_the_real_code_usable() {
        let pairing = Pairing::new();
        let now = Instant::now();
        let code = pairing.issue(now, fixed(0));

        assert!(!pairing.redeem("ZZZZZZ", now));
        assert!(pairing.redeem(&code, now), "the real code must still work");
    }

    #[test]
    fn issuing_again_replaces_the_previous_code() {
        let pairing = Pairing::new();
        let now = Instant::now();
        let first = pairing.issue(now, fixed(0));
        let second = pairing.issue(now, fixed(9));

        assert_ne!(first, second);
        assert!(!pairing.redeem(&first, now), "the old code must be dead");
        assert!(pairing.redeem(&second, now));
    }

    #[test]
    fn redeeming_without_an_outstanding_code_fails() {
        let pairing = Pairing::new();
        assert!(!pairing.redeem("ACDEFG", Instant::now()));
    }

    /// Codes are read aloud and typed by hand, so the alphabet excludes the
    /// characters people confuse. A code containing them would surface as
    /// "pairing is broken" rather than as a typo.
    #[test]
    fn the_alphabet_omits_ambiguous_characters() {
        for ambiguous in *b"O0I1LS5B8" {
            assert!(
                !ALPHABET.contains(&ambiguous),
                "{} is easily misread",
                ambiguous as char
            );
        }
    }

    /// The defect this caught in practice: a low-entropy source produced
    /// `AAAAAA`. A code whose characters are all equal has no security left,
    /// so the generator must spread distinct input bytes across distinct
    /// characters.
    #[test]
    fn distinct_entropy_produces_distinct_characters() {
        let pairing = Pairing::new();
        let code = pairing.issue(Instant::now(), fixed(0));
        assert!(
            code.chars().collect::<std::collections::HashSet<_>>().len() > 1,
            "every character identical means the entropy was not used: {code}"
        );
    }

    #[test]
    fn a_generated_code_uses_only_the_alphabet() {
        let pairing = Pairing::new();
        let code = pairing.issue(Instant::now(), fixed(0));
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|byte| ALPHABET.contains(&byte)));
    }
}
