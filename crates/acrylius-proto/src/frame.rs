//! The one tag byte outside the encryption: it tells a responder which
//! handshake to build. Forgeable, but the mode is also mixed into the Noise
//! prologue, so flipping it gives a decrypt failure, not a downgrade.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum FrameKind {
    /// A message of an `XX` pairing handshake. Anybody may send one; the core's
    /// admission policy is what bounds it.
    PairHandshake = 1,
    /// A message of an `IKpsk2` session handshake.
    SessionHandshake = 2,
    /// An encrypted envelope on an established session.
    Transport = 3,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame is empty")]
    Empty,
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
}

impl FrameKind {
    fn from_byte(b: u8) -> Result<Self, FrameError> {
        match b {
            1 => Ok(Self::PairHandshake),
            2 => Ok(Self::SessionHandshake),
            3 => Ok(Self::Transport),
            other => Err(FrameError::UnknownKind(other)),
        }
    }
}

/// Prefix `payload` with its kind.
#[must_use]
pub fn join(kind: FrameKind, payload: &[u8]) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(payload.len() + 1);
    out.push(kind as u8);
    out.extend_from_slice(payload);
    out
}

/// Split a received frame into its kind and body.
pub fn split(bytes: &[u8]) -> Result<(FrameKind, &[u8]), FrameError> {
    let (&first, rest) = bytes.split_first().ok_or(FrameError::Empty)?;
    Ok((FrameKind::from_byte(first)?, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_kind() {
        for k in [
            FrameKind::PairHandshake,
            FrameKind::SessionHandshake,
            FrameKind::Transport,
        ] {
            let framed = join(k, b"payload");
            assert_eq!(split(&framed).unwrap(), (k, &b"payload"[..]));
        }
    }

    #[test]
    fn an_empty_payload_still_carries_its_kind() {
        // Noise's first XX message can have an empty payload.
        let framed = join(FrameKind::PairHandshake, b"");
        assert_eq!(
            split(&framed).unwrap(),
            (FrameKind::PairHandshake, &b""[..])
        );
    }

    #[test]
    fn rejects_empty_and_unknown() {
        assert_eq!(split(b""), Err(FrameError::Empty));
        assert_eq!(split(&[0u8]), Err(FrameError::UnknownKind(0)));
        assert_eq!(split(&[9u8]), Err(FrameError::UnknownKind(9)));
    }
}
