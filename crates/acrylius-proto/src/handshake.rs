//! Handshake payloads — what rides inside the Noise messages.
//!
//! ## Why there is no command in here
//!
//! `IKpsk2`'s first message is encrypted under `es` + `ss`, which means it is
//! **not forward-secret**: someone who later compromises the responder's static
//! key can decrypt every message 1 they ever recorded. Capability lists and a
//! device name are semi-public and survive that fine. An unlock command would
//! not. So message 1 carries identity and capabilities, and nothing else; the
//! initiator waits for message 2 before sending anything that matters. On a LAN
//! that costs about four milliseconds.
//!
//! ## Why there is a timestamp
//!
//! `IKpsk2` message 1 is **replayable** — an observer can record one and send it
//! again later. WireGuard solves this with a monotonic timestamp plus a
//! per-peer greatest-seen check, and so do we ([`Hello::check_freshness`]).
//!
//! This is where the old project's entire replay apparatus goes. `pc-helper-ios`
//! needed a persisted SQLite table of every nonce inside a 30-second window,
//! swept on a timer, because a signed request carries no session. Here the Noise
//! session's own cipher counter handles replay *within* a session, and one `u64`
//! per peer handles replay *of the session opener*. One integer replaces a table.

use alloc::string::String;
use alloc::vec::Vec;

/// How far a peer's clock may differ from ours before we refuse the handshake.
///
/// Generous, because this is not a freshness guarantee — [`GreatestSeen`] is.
/// It only bounds how far into the future a peer can push its own watermark and
/// lock itself out after a clock correction.
pub const MAX_SKEW_MS: u64 = 60_000;

#[derive(Clone, PartialEq, Eq, Debug, minicbor::Encode, minicbor::Decode)]
pub struct Hello {
    #[n(0)]
    pub v: u8,
    /// Sender's clock, milliseconds since the Unix epoch.
    #[n(1)]
    pub ts_ms: u64,
    /// Derived by the *receiver* from the static key Noise just authenticated;
    /// carried here only so a log line can name the peer before the lookup.
    /// Never trusted as an identity — see [`crate::ids`].
    #[b(2)]
    pub device_id: String,
    #[b(3)]
    pub name: String,
    #[b(4)]
    pub platform: String,
    /// Capabilities this side may send.
    #[n(5)]
    pub caps_out: Vec<String>,
    /// Capabilities this side can handle.
    #[n(6)]
    pub caps_in: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FreshnessError {
    #[error("handshake timestamp is {0} ms outside the permitted skew")]
    Skew(u64),
    #[error("handshake timestamp {got} is not newer than the last seen {seen} — replay")]
    Replay { got: u64, seen: u64 },
}

/// The per-peer replay watermark. Persisted; a restart must not reopen the window.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct GreatestSeen(pub u64);

impl Hello {
    /// Reject a stale, skewed, or replayed handshake opener.
    ///
    /// Both checks are needed and neither subsumes the other: the watermark stops
    /// replay, and the skew bound stops a peer with a wildly wrong clock from
    /// setting a watermark so far ahead that it can never connect again.
    pub fn check_freshness(
        &self,
        now_ms: u64,
        seen: GreatestSeen,
    ) -> Result<GreatestSeen, FreshnessError> {
        let delta = now_ms.abs_diff(self.ts_ms);
        if delta > MAX_SKEW_MS {
            return Err(FreshnessError::Skew(delta - MAX_SKEW_MS));
        }
        // Strictly greater: replaying the *same* opener must fail, not tie.
        if self.ts_ms <= seen.0 {
            return Err(FreshnessError::Replay { got: self.ts_ms, seen: seen.0 });
        }
        Ok(GreatestSeen(self.ts_ms))
    }
}

/// The capabilities that may flow from `sender` to `receiver`.
///
/// A plain set intersection, which is the entire reason the major version lives
/// *inside* the capability id: `org.acrylius.clipboard/2` is simply a different
/// string from `/1`, so there is no separate version field to compare wrongly.
///
/// Note this is directional. `a.negotiate(b) != b.negotiate(a)` in general, and
/// conflating the two would silently let a peer send a capability it only
/// declared it could receive.
#[must_use]
pub fn negotiate(sender_out: &[String], receiver_in: &[String]) -> Vec<String> {
    let mut caps: Vec<String> = sender_out
        .iter()
        .filter(|c| receiver_in.contains(c))
        .cloned()
        .collect();
    caps.sort_unstable();
    caps.dedup();
    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn hello(ts_ms: u64) -> Hello {
        Hello {
            v: crate::WIRE_VERSION,
            ts_ms,
            device_id: "x".to_string(),
            name: "test".to_string(),
            platform: "linux".to_string(),
            caps_out: vec![],
            caps_in: vec![],
        }
    }

    #[test]
    fn round_trips() {
        let mut h = hello(1_700_000_000_000);
        h.caps_out = vec!["org.acrylius.session/1".to_string()];
        h.caps_in = vec!["org.acrylius.clipboard/1".to_string()];
        let bytes = minicbor::to_vec(&h).unwrap();
        assert_eq!(minicbor::decode::<Hello>(&bytes).unwrap(), h);
    }

    #[test]
    fn a_fresh_handshake_advances_the_watermark() {
        let now = 1_700_000_000_000;
        let seen = hello(now).check_freshness(now, GreatestSeen(0)).unwrap();
        assert_eq!(seen, GreatestSeen(now));
    }

    #[test]
    fn replaying_the_same_opener_is_refused() {
        let now = 1_700_000_000_000;
        let h = hello(now);
        let seen = h.check_freshness(now, GreatestSeen(0)).unwrap();
        // The exact bytes an observer recorded, sent again a moment later.
        assert_eq!(
            h.check_freshness(now + 500, seen),
            Err(FreshnessError::Replay { got: now, seen: now })
        );
    }

    #[test]
    fn an_older_opener_is_refused_even_within_skew() {
        let now = 1_700_000_000_000;
        let seen = GreatestSeen(now);
        assert!(matches!(
            hello(now - 1_000).check_freshness(now, seen),
            Err(FreshnessError::Replay { .. })
        ));
    }

    #[test]
    fn wild_clocks_are_refused_in_both_directions() {
        let now = 1_700_000_000_000;
        assert!(matches!(
            hello(now + MAX_SKEW_MS + 1).check_freshness(now, GreatestSeen(0)),
            Err(FreshnessError::Skew(1))
        ));
        assert!(matches!(
            hello(now - MAX_SKEW_MS - 1).check_freshness(now, GreatestSeen(0)),
            Err(FreshnessError::Skew(1))
        ));
    }

    #[test]
    fn the_watermark_survives_a_restart() {
        // The whole point of persisting it: a daemon that forgot would accept a
        // recorded opener again. Reloading the stored value must still refuse.
        let now = 1_700_000_000_000;
        let h = hello(now);
        let persisted = h.check_freshness(now, GreatestSeen(0)).unwrap();
        let after_restart = GreatestSeen(persisted.0);
        assert!(h.check_freshness(now + 10, after_restart).is_err());
    }

    #[test]
    fn negotiation_is_an_intersection_and_is_directional() {
        let a_out = vec!["s/1".to_string(), "c/1".to_string()];
        let b_in = vec!["c/1".to_string(), "w/1".to_string()];
        assert_eq!(negotiate(&a_out, &b_in), vec!["c/1".to_string()]);
        // Nothing flows the other way: b declared it can RECEIVE these, not send.
        assert!(negotiate(&b_in, &[]).is_empty());
    }

    #[test]
    fn a_different_major_version_is_a_different_capability() {
        let out = vec!["org.acrylius.clipboard/2".to_string()];
        let inn = vec!["org.acrylius.clipboard/1".to_string()];
        assert!(negotiate(&out, &inn).is_empty(), "/2 must not satisfy /1");
    }
}
