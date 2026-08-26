//! The bulk side channel: a key, a frame, and nothing else.
//!
//! A file does not travel through the envelope. Every chunk would cross the FFI
//! boundary twice and be copied twice, and a hundred-megabyte transfer would
//! stall every other message on the link behind it. So bulk bytes go over their
//! own connection, negotiated in-band and then left alone: the core is not
//! involved once it has handed out a key.
//!
//! That key is the whole security story. It is derived from the Noise session
//! both sides already share, so a connection nobody else can produce ciphertext
//! for is one only the peer could have opened — the channel needs no handshake
//! and no identity of its own. A transfer id keys the derivation so two
//! transfers on one session never share a key, and the id itself is not secret:
//! it travels in the clear so a listener can tell which transfer a connection
//! belongs to before it can decrypt anything.

//! ## Why the sealing is here and not in a transport
//!
//! Because there are two transports. The daemon moves these frames over tokio;
//! a phone moves them over a blocking socket, since nothing async may cross the
//! FFI boundary. If each carried its own idea of how a chunk is sealed there
//! would be two implementations of the wire format, which is the single thing
//! this project exists to avoid. So the format lives here as plain buffer
//! transforms — no socket, no state, no runtime — and each transport only
//! decides how bytes reach the wire.

use alloc::vec::Vec;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

const BULK_INFO: &[u8] = b"acrylius/bulk/v1";

/// Largest chunk that may be sent at once.
///
/// A size, not a limit on the file: a transfer is as many of these as it takes.
/// 64 KiB keeps the per-chunk overhead irrelevant while staying small enough
/// that a phone is not asked to hold much at once.
pub const CHUNK: usize = 64 * 1024;

/// The most a single frame may claim, so a peer cannot name a length and make
/// the other side reserve it before sending anything.
pub const MAX_FRAME: u32 = (CHUNK + 64) as u32;

/// The key for one transfer.
///
/// Derived from the session's handshake hash and the transfer id, so two
/// transfers on one session never share a key and a key learned from one says
/// nothing about another.
#[must_use]
pub fn key(handshake_hash: &[u8], transfer: u64) -> [u8; 32] {
    let mut info = BULK_INFO.to_vec();
    info.extend_from_slice(&transfer.to_be_bytes());
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, handshake_hash)
        .expand(&info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// What a dialer says before any ciphertext, so a listener can tell which
/// transfer this connection is for.
///
/// Not secret, and not trusted: naming a transfer gets a connection no further
/// than the key for it allows, and an impostor cannot produce a chunk that
/// decrypts.
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

/// A nonce from a chunk's sequence number.
///
/// The whole nonce is the counter, so two chunks of one transfer can never
/// share one. Reusing a nonce under one key is the failure this shape makes
/// impossible rather than merely unlikely — and it is safe to count from zero
/// only because the key is fresh for every transfer.
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

/// Open one chunk that arrived.
///
/// A chunk that will not open is the end of the transfer, not a chunk to skip:
/// the sequence number is in the nonce, so reordering, repeating or dropping
/// one makes every chunk after it fail too.
pub fn open(key: &[u8], seq: u64, frame: &[u8]) -> Result<Vec<u8>, ChunkError> {
    cipher(key)?
        .decrypt(Nonce::from_slice(&nonce(seq)), frame)
        .map_err(|_| ChunkError::Sealed(seq))
}

fn cipher(key: &[u8]) -> Result<ChaCha20Poly1305, ChunkError> {
    let key: [u8; 32] = key.try_into().map_err(|_| ChunkError::BadKey)?;
    Ok(ChaCha20Poly1305::new((&key).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_round_trips() {
        let k = key(b"hh", 1);
        let sealed = seal(&k, 0, b"hello").unwrap();
        assert_eq!(open(&k, 0, &sealed).unwrap(), b"hello");
    }

    #[test]
    fn a_chunk_will_not_open_under_another_transfers_key() {
        let sealed = seal(&key(b"hh", 1), 0, b"hello").unwrap();
        assert_eq!(
            open(&key(b"hh", 2), 0, &sealed),
            Err(ChunkError::Sealed(0)),
            "a key for another transfer opens nothing"
        );
    }

    #[test]
    fn a_chunk_will_not_open_at_the_wrong_position() {
        // What makes reordering, repetition and truncation all one failure.
        let k = key(b"hh", 1);
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
        assert_ne!(key(hh, 1), key(hh, 2));
    }

    #[test]
    fn two_sessions_do_not_share_a_key_for_the_same_transfer_id() {
        // Transfer ids restart per session, so without the handshake hash in
        // the derivation a recorded transfer could be replayed into a later one.
        assert_ne!(key(b"session one", 1), key(b"session two", 1));
    }

    #[test]
    fn a_bulk_key_is_not_the_session_psk() {
        // Both derive from a handshake hash. Without the info string they would
        // be the same bytes, and a transfer key handed to a host — which is the
        // point of deriving one — would be the session's own key.
        let hh = b"one handshake";
        assert_ne!(key(hh, 0), crate::pairing::session_psk(hh));
    }

    #[test]
    fn the_same_inputs_give_the_same_key() {
        // Both ends derive independently; nothing is transmitted.
        assert_eq!(key(b"hh", 7), key(b"hh", 7));
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
}
