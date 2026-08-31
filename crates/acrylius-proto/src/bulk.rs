//! The bulk side channel: file bytes travel over their own connection, keyed
//! from the Noise session both sides already share. The sealing lives here so
//! both transports speak one wire format.

use alloc::vec::Vec;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

const BULK_INFO: &[u8] = b"acrylius/bulk/v1";

/// Largest chunk sent at once; a transfer is as many of these as it takes.
pub const CHUNK: usize = 64 * 1024;

/// The most a single frame may claim, so a peer cannot reserve memory it never sends.
pub const MAX_FRAME: u32 = (CHUNK + 64) as u32;

/// The key for one transfer. `offerer` must be in the derivation: each end
/// numbers transfers from 1, so keying on the id alone gives the two directions
/// the same key and nonce — keystream reuse. Length-prefixed so
/// `offerer || transfer` cannot be read two ways.
#[must_use]
pub fn key(handshake_hash: &[u8], offerer: &str, transfer: u64) -> [u8; 32] {
    let mut info = BULK_INFO.to_vec();
    info.push(u8::try_from(offerer.len()).unwrap_or(u8::MAX));
    info.extend_from_slice(offerer.as_bytes());
    info.extend_from_slice(&transfer.to_be_bytes());
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, handshake_hash)
        .expand(&info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// What a dialer says before any ciphertext: which transfer this connection is
/// for. Not secret, and not trusted.
#[must_use]
pub fn hello(transfer: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[..4].copy_from_slice(b"ACRB");
    out[4..].copy_from_slice(&transfer.to_be_bytes());
    out
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum HelloError {
    #[error("not an acrylius bulk connection")]
    NotOurs,
}

/// Read a dialer's opening bytes.
pub fn read_hello(bytes: &[u8; 12]) -> Result<u64, HelloError> {
    if &bytes[..4] != b"ACRB" {
        return Err(HelloError::NotOurs);
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[4..]);
    Ok(u64::from_be_bytes(id))
}

/// The nonce is the sequence number; counting from zero is safe only because
/// the key is fresh for every transfer.
#[must_use]
pub fn nonce(seq: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[4..].copy_from_slice(&seq.to_be_bytes());
    out
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    #[error("a bulk key is 32 bytes")]
    BadKey,
    #[error("chunk {0} would not open: wrong key, or a frame out of order")]
    Sealed(u64),
    #[error("chunk {0} could not be sealed")]
    Unsealed(u64),
}

/// Seal one chunk for sending.
pub fn seal(key: &[u8], seq: u64, plaintext: &[u8]) -> Result<Vec<u8>, ChunkError> {
    cipher(key)?
        .encrypt(Nonce::from_slice(&nonce(seq)), plaintext)
        .map_err(|_| ChunkError::Unsealed(seq))
}

/// Open one chunk. A chunk that will not open ends the transfer, not just the
/// chunk: the sequence number is in the nonce, so every later chunk fails too.
pub fn open(key: &[u8], seq: u64, frame: &[u8]) -> Result<Vec<u8>, ChunkError> {
    cipher(key)?
        .decrypt(Nonce::from_slice(&nonce(seq)), frame)
        .map_err(|_| ChunkError::Sealed(seq))
}

fn cipher(key: &[u8]) -> Result<ChaCha20Poly1305, ChunkError> {
    let key: [u8; 32] = key.try_into().map_err(|_| ChunkError::BadKey)?;
    Ok(ChaCha20Poly1305::new((&key).into()))
}

/// A file name from a peer, made safe to use: anything that could steer where
/// the bytes land is removed rather than rejected. Shared by every receiver so
/// the rule cannot diverge.
#[must_use]
pub fn safe_name(offered: &str) -> alloc::string::String {
    use alloc::string::{String, ToString};

    // Control characters come out before the dot trim: "\u{0}.." must not survive as "..".
    let base: String = offered
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(offered)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = base.trim().trim_start_matches('.').trim();

    // Bounded in bytes: filesystems stop at 255, and room is left for `.part` and ` (2)`.
    let mut cleaned = String::new();
    for c in trimmed.chars() {
        if cleaned.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        cleaned.push(c);
    }
    if cleaned.is_empty() {
        "received".to_string()
    } else {
        cleaned
    }
}

/// The most a made-safe name may take on disk.
const MAX_NAME_BYTES: usize = 200;

#[cfg(test)]
mod tests {
    use super::*;

    /// Two device ids, standing in for the two ends of a session.
    const A: &str = "AAAAAAAAAAAAAAAAAAAAAA";
    const B: &str = "BBBBBBBBBBBBBBBBBBBBBB";

    #[test]
    fn the_two_directions_of_one_session_do_not_share_a_key() {
        // Each end numbers transfers from 1; keyed on the id alone the two
        // directions share a key and nonce zero.
        let hh = b"one session, two directions";
        assert_ne!(
            key(hh, A, 1),
            key(hh, B, 1),
            "the same id offered from each end must not derive the same key"
        );

        let sealed = seal(&key(hh, A, 1), 0, b"a secret file").unwrap();
        assert_eq!(
            open(&key(hh, B, 1), 0, &sealed),
            Err(ChunkError::Sealed(0)),
            "the other direction's key must open nothing"
        );
    }

    #[test]
    fn the_offerer_is_length_prefixed_so_it_cannot_run_into_the_id() {
        // Without the length byte, ("AB", 1) and ("A", …) could produce the same info string.
        assert_ne!(key(b"hh", "AB", 1), key(b"hh", "A", 1));
    }

    #[test]
    fn a_name_cannot_hide_dots_behind_a_control_character() {
        // Trimming dots before filtering controls let "\u{0}.." come back as "..".
        assert_eq!(safe_name("\u{0}.."), "received");
        assert_eq!(safe_name("\u{1}."), "received");
        assert_eq!(safe_name("\u{7}../etc/passwd"), "passwd");
        assert_eq!(safe_name("\u{0}.bashrc"), "bashrc");
        // And the ordinary case is untouched.
        assert_eq!(safe_name("holiday.jpg"), "holiday.jpg");
    }

    #[test]
    fn a_name_is_bounded_in_bytes_because_a_filesystem_is() {
        // 200 emoji is 800 bytes; a name over 255 bytes cannot be created at all.
        let long = "🙂".repeat(200);
        let safe = safe_name(&long);
        assert!(safe.len() <= 200, "{} bytes", safe.len());
        assert!(!safe.is_empty());
        // Cut on a character boundary, never through one.
        assert!(safe.chars().all(|c| c == '🙂'));
    }

    #[test]
    fn a_chunk_round_trips() {
        let k = key(b"hh", A, 1);
        let sealed = seal(&k, 0, b"hello").unwrap();
        assert_eq!(open(&k, 0, &sealed).unwrap(), b"hello");
    }

    #[test]
    fn a_chunk_will_not_open_under_another_transfers_key() {
        let sealed = seal(&key(b"hh", A, 1), 0, b"hello").unwrap();
        assert_eq!(
            open(&key(b"hh", A, 2), 0, &sealed),
            Err(ChunkError::Sealed(0)),
            "a key for another transfer opens nothing"
        );
    }

    #[test]
    fn a_chunk_will_not_open_at_the_wrong_position() {
        // What makes reordering, repetition and truncation all one failure.
        let k = key(b"hh", A, 1);
        let sealed = seal(&k, 3, b"hello").unwrap();
        assert_eq!(open(&k, 4, &sealed), Err(ChunkError::Sealed(4)));
        assert_eq!(open(&k, 2, &sealed), Err(ChunkError::Sealed(2)));
    }

    #[test]
    fn a_nonce_is_the_sequence_number_and_nothing_else() {
        assert_ne!(nonce(0), nonce(1));
        assert_eq!(nonce(0), [0u8; 12]);
        assert_eq!(&nonce(1)[4..], &1u64.to_be_bytes());
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused_rather_than_padded() {
        assert_eq!(seal(b"short", 0, b"x"), Err(ChunkError::BadKey));
    }

    #[test]
    fn two_transfers_on_one_session_do_not_share_a_key() {
        let hh = b"the same session";
        assert_ne!(key(hh, A, 1), key(hh, A, 2));
    }

    #[test]
    fn two_sessions_do_not_share_a_key_for_the_same_transfer_id() {
        // Transfer ids restart per session; the handshake hash keeps replays out.
        assert_ne!(key(b"session one", A, 1), key(b"session two", A, 1));
    }

    #[test]
    fn a_bulk_key_is_not_the_session_psk() {
        // Without the info string a transfer key handed to a host would be the session's own key.
        let hh = b"one handshake";
        assert_ne!(key(hh, A, 0), crate::pairing::session_psk(hh));
    }

    #[test]
    fn the_same_inputs_give_the_same_key() {
        // Both ends derive independently; nothing is transmitted.
        assert_eq!(key(b"hh", A, 7), key(b"hh", A, 7));
    }

    #[test]
    fn hello_round_trips() {
        for id in [0, 1, u64::MAX] {
            assert_eq!(read_hello(&hello(id)).unwrap(), id);
        }
    }

    #[test]
    fn something_else_dialling_the_port_is_refused() {
        let mut junk = [0u8; 12];
        junk[..4].copy_from_slice(b"GET ");
        assert_eq!(read_hello(&junk), Err(HelloError::NotOurs));
    }

    #[test]
    fn a_name_from_a_peer_cannot_choose_a_directory() {
        assert_eq!(safe_name("../../.bashrc"), "bashrc");
        assert_eq!(safe_name("/etc/passwd"), "passwd");
        assert_eq!(safe_name(r"C:\windows\system32\x.dll"), "x.dll");
        assert_eq!(safe_name("holiday.jpg"), "holiday.jpg");
    }

    #[test]
    fn a_name_that_is_nothing_useful_still_gets_one() {
        assert_eq!(safe_name(""), "received");
        assert_eq!(safe_name("   "), "received");
        assert_eq!(safe_name("../.."), "received");
    }

    #[test]
    fn a_control_character_does_not_survive() {
        assert_eq!(safe_name("in\u{1b}[2Kvoice.pdf"), "in[2Kvoice.pdf");
        assert!(!safe_name("a\nb.txt").contains('\n'));
    }
}
