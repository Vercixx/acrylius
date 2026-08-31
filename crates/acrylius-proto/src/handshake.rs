//! Handshake payloads: what rides inside the Noise messages.
//!
//! `IKpsk2` message 1 is not forward-secret, so it carries only identity and
//! capabilities. It is also replayable, so a monotonic timestamp plus a
//! per-peer greatest-seen watermark refuses recorded openers (WireGuard's scheme).

use alloc::string::String;
use alloc::vec::Vec;

/// Permitted clock skew. Not a freshness guarantee ([`GreatestSeen`] is); it
/// bounds how far ahead a peer can push its own watermark and lock itself out.
pub const MAX_SKEW_MS: u64 = 60_000;

#[derive(Clone, PartialEq, Eq, Debug, minicbor::Encode, minicbor::Decode)]
pub struct Hello {
    #[n(0)]
    pub v: u8,
    /// Sender's clock, milliseconds since the Unix epoch.
    #[n(1)]
    pub ts_ms: u64,
    /// Carried only so a log line can name the peer; never trusted — the
    /// receiver derives the id from the authenticated static key.
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
    #[error("handshake timestamp {got} is not newer than the last seen {seen}: replay")]
    Replay { got: u64, seen: u64 },
}

/// The per-peer replay watermark. Persisted; a restart must not reopen the window.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct GreatestSeen(pub u64);

impl Hello {
    /// Reject a stale, skewed, or replayed handshake opener. The watermark
    /// stops replay; the skew bound stops a wild clock from setting a
    /// watermark it can never pass again.
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
            return Err(FreshnessError::Replay {
                got: self.ts_ms,
                seen: seen.0,
            });
        }
        Ok(GreatestSeen(self.ts_ms))
    }
}

/// The capabilities that may flow from `sender` to `receiver`: a plain set
/// intersection. Directional — `a.negotiate(b) != b.negotiate(a)` in general.
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
            Err(FreshnessError::Replay {
                got: now,
                seen: now
            })
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
    fn a_clock_exactly_at_the_limit_is_still_within_it() {
        // The check must stay `>`, not `>=`: a clock exactly a minute out is within bound.
        let now = 1_700_000_000_000;
        assert!(
            hello(now + MAX_SKEW_MS)
                .check_freshness(now, GreatestSeen(0))
                .is_ok()
        );
        assert!(
            hello(now - MAX_SKEW_MS)
                .check_freshness(now, GreatestSeen(0))
                .is_ok()
        );
    }

    #[test]
    fn a_refusal_says_how_far_out_the_clock_was() {
        // The error carries the overshoot; pinned where wrong arithmetic would differ.
        let now = 1_700_000_000_000;
        assert_eq!(
            hello(now + MAX_SKEW_MS * 2).check_freshness(now, GreatestSeen(0)),
            Err(FreshnessError::Skew(MAX_SKEW_MS))
        );
    }

    #[test]
    fn the_watermark_survives_a_restart() {
        // A daemon that forgot the watermark would accept a recorded opener again.
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
