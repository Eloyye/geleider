//! Attested Session destinations. A Run result is queued as a prompt in the
//! Session it names, so the daemon accepts that destination only when a Harness
//! hook vouched for it: the hook sends the daemon a single-use nonce with the
//! Session it saw, and puts the same nonce in the submit_run arguments. Claim and
//! release calls name their Session the same way.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::session::SessionKey;

/// Random bytes in a nonce; it is written as twice as many hex digits.
const NONCE_BYTES: usize = 32;
/// Outstanding attestations kept; the oldest goes first beyond this.
const MAX_PENDING: usize = 1024;

/// A hook's single-use proof that it, not the model, chose a destination.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Nonce(String);

impl Nonce {
    /// A fresh nonce from the system's random source; `None` if it is unavailable.
    pub fn generate() -> Option<Self> {
        let mut bytes = [0u8; NONCE_BYTES];
        // The standard library has no stable secure random source; getentropy is
        // the system's, and cannot fail for a buffer of at most 256 bytes.
        // SAFETY: `bytes` is a valid, writable buffer of the length passed.
        let filled = unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } == 0;
        filled.then(|| Self(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }
}

impl TryFrom<String> for Nonce {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let valid = text.len() == NONCE_BYTES * 2
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(text))
        } else {
            Err("invalid nonce".into())
        }
    }
}

impl From<Nonce> for String {
    fn from(nonce: Nonce) -> Self {
        nonce.0
    }
}

/// The `session` of a submission: where its result should go and, when a hook
/// set it, the nonce that hook attested it with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    #[serde(flatten)]
    pub key: SessionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<Nonce>,
}

/// What a hook tells the daemon before the call it annotated goes ahead.
#[derive(Debug, Serialize, Deserialize)]
pub struct Attestation {
    pub session: SessionKey,
    pub nonce: Nonce,
}

/// Attestations waiting for the submission that carries their nonce.
pub struct Attestations {
    /// How long a hook's attestation waits; long enough for a permission prompt.
    ttl: Duration,
    pending: Mutex<Pending>,
    arrived: Condvar,
}

#[derive(Default)]
struct Pending {
    /// Each nonce's Session, when it expires, and the order it arrived in.
    by_nonce: HashMap<Nonce, (SessionKey, Instant, u64)>,
    arrivals: u64,
}

impl Attestations {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            pending: Mutex::new(Pending::default()),
            arrived: Condvar::new(),
        }
    }

    pub fn record(&self, attestation: Attestation) {
        let now = Instant::now();
        let mut pending = self.lock();
        let Pending { by_nonce, arrivals } = &mut *pending;
        if by_nonce.len() >= MAX_PENDING {
            by_nonce.retain(|_, (_, expires, _)| *expires > now);
        }
        if by_nonce.len() >= MAX_PENDING {
            let oldest = by_nonce
                .iter()
                .min_by_key(|(_, (_, _, arrival))| *arrival)
                .map(|(nonce, _)| nonce.clone());
            if let Some(nonce) = oldest {
                by_nonce.remove(&nonce);
            }
        }
        *arrivals += 1;
        by_nonce.insert(
            attestation.nonce,
            (attestation.session, now + self.ttl, *arrivals),
        );
        self.arrived.notify_all();
    }

    /// Whether a hook attested `destination`, using its nonce up either way. Waits
    /// up to `wait` for the attestation, which travels separately from the call.
    pub fn consume(&self, destination: &Destination, wait: Duration) -> bool {
        let Some(nonce) = &destination.nonce else {
            return false;
        };
        let deadline = Instant::now() + wait;
        let mut pending = self.lock();
        loop {
            let now = Instant::now();
            // Removed even when it names another Session: a nonce is good for one try.
            if let Some((session, expires, _)) = pending.by_nonce.remove(nonce) {
                return session == destination.key && now < expires;
            }
            if now >= deadline {
                return false;
            }
            pending = self
                .arrived
                .wait_timeout(pending, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::session::Harness;

    const NO_WAIT: Duration = Duration::ZERO;

    fn key(id: &str) -> SessionKey {
        SessionKey::new(Harness::Codex, id).unwrap()
    }

    fn attest(attestations: &Attestations, id: &str) -> Nonce {
        let nonce = Nonce::generate().unwrap();
        attestations.record(Attestation {
            session: key(id),
            nonce: nonce.clone(),
        });
        nonce
    }

    fn to(id: &str, nonce: Option<&Nonce>) -> Destination {
        Destination {
            key: key(id),
            nonce: nonce.cloned(),
        }
    }

    #[test]
    fn nonces_are_random_lowercase_hex() {
        let (a, b) = (Nonce::generate().unwrap(), Nonce::generate().unwrap());
        assert_ne!(a, b);
        assert!(Nonce::try_from(String::from(a.clone())).is_ok());
        for text in [
            String::new(),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(63),
        ] {
            assert!(Nonce::try_from(text).is_err());
        }
        let parsed: Destination =
            serde_json::from_value(serde_json::json!({"harness":"codex","id":"s","nonce":a}))
                .unwrap();
        assert_eq!(parsed.nonce, Some(a));
        let forged = serde_json::json!({"harness":"codex","id":"s","nonce":"not-hex"});
        assert!(serde_json::from_value::<Destination>(forged).is_err());
    }

    #[test]
    fn an_attested_destination_is_accepted_once() {
        let attestations = Attestations::new(Duration::from_secs(60));
        let nonce = attest(&attestations, "caller");
        assert!(attestations.consume(&to("caller", Some(&nonce)), NO_WAIT));
        // Replayed.
        assert!(!attestations.consume(&to("caller", Some(&nonce)), NO_WAIT));
    }

    #[test]
    fn forged_destinations_are_refused() {
        let attestations = Attestations::new(Duration::from_secs(60));
        // No nonce, as a model writes one, and a nonce no hook sent.
        assert!(!attestations.consume(&to("victim", None), NO_WAIT));
        let made_up = Nonce::generate().unwrap();
        assert!(!attestations.consume(&to("victim", Some(&made_up)), NO_WAIT));
        // A real nonce with another Session's id, after which it is used up.
        let nonce = attest(&attestations, "caller");
        assert!(!attestations.consume(&to("victim", Some(&nonce)), NO_WAIT));
        assert!(!attestations.consume(&to("caller", Some(&nonce)), NO_WAIT));
    }

    #[test]
    fn expired_attestations_are_refused() {
        let attestations = Attestations::new(Duration::ZERO);
        let nonce = attest(&attestations, "caller");
        assert!(!attestations.consume(&to("caller", Some(&nonce)), NO_WAIT));
    }

    #[test]
    fn a_submission_waits_briefly_for_its_attestation() {
        let attestations = Arc::new(Attestations::new(Duration::from_secs(60)));
        let nonce = Nonce::generate().unwrap();
        let late = Arc::clone(&attestations);
        let sent = nonce.clone();
        let hook = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            late.record(Attestation {
                session: key("caller"),
                nonce: sent,
            });
        });
        assert!(attestations.consume(&to("caller", Some(&nonce)), Duration::from_secs(5)));
        hook.join().unwrap();

        let start = Instant::now();
        let never = Nonce::generate().unwrap();
        let wait = Duration::from_millis(50);
        assert!(!attestations.consume(&to("caller", Some(&never)), wait));
        assert!(start.elapsed() >= wait);
    }

    #[test]
    fn outstanding_attestations_are_bounded() {
        let attestations = Attestations::new(Duration::from_secs(60));
        let first = attest(&attestations, "caller");
        let mut last = None;
        for _ in 0..MAX_PENDING {
            last = Some(attest(&attestations, "caller"));
        }
        assert_eq!(attestations.lock().by_nonce.len(), MAX_PENDING);
        assert!(!attestations.consume(&to("caller", Some(&first)), NO_WAIT));
        assert!(attestations.consume(&to("caller", last.as_ref()), NO_WAIT));
    }
}
