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
/// Derived from the session's handshake hash, **who offered the transfer**, and
/// the transfer id, so two transfers on one session never share a key and a key
/// learned from one says nothing about another.
///
/// `offerer` is not decoration. Each end allocates transfer ids from its own
/// counter starting at one, so the first file a phone sends and the first file
/// the desktop sends carry the same id. Keyed on the id alone, those two
/// transfers got the same key *and* both began at nonce zero, which is
/// keystream reuse: xor the two ciphertexts and both files fall out, on a
/// channel that is a plain TCP connection outside the Noise session. The
/// one-time Poly1305 key is reused with it, so the authentication goes too.
///
/// The offerer's device id is the discriminator because it is the one thing
/// both ends agree on without another round trip: the sender knows it offered,
/// and the receiver knows the offer came from its peer. A relative label like
/// "am I sending" would have given the two ends different keys.
///
/// Length-prefixed so `offerer || transfer` cannot be read two ways.
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

/// A file name from a peer, made safe to use.
///
/// A peer chooses what to call its file and nothing else. Anything that could
/// steer where the bytes land is removed rather than rejected, because a
/// refusal over a stray slash helps nobody: `../../.bashrc` becomes `.bashrc`
/// in the directory that was going to be used anyway.
///
/// Here rather than beside a filesystem, because every device that can receive
/// a file needs this rule and they do not share a filesystem — a phone writes
/// into an app container and a desktop into a configured directory. What they
/// must share is the answer, since a second copy of this is how one of them
/// ends up with the path traversal the other does not.
#[must_use]
pub fn safe_name(offered: &str) -> alloc::string::String {
    use alloc::string::{String, ToString};

    // Control characters come out first, not last.
    //
    // Stripping the dots before them meant a name could hide behind one: the
    // leading character of `"\u{0}.."` is not a dot, so nothing was trimmed,
    // and the filter then removed the NUL and handed back `".."`. That is
    // exactly the kind of name this function exists to never return.
    let base: String = offered
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(offered)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = base.trim().trim_start_matches('.').trim();

    // Bounded in bytes rather than characters. A file name is stored as bytes
    // and most filesystems stop at 255 of them, so 120 characters was up to 480
    // — a name the receiver cannot create, failing the transfer with an errno
    // instead of a reason. Short of the limit, to leave room for the `.part`
    // suffix and a ` (2)` if the name is taken.
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
        // The bug this exists to keep out. Each end numbers its own transfers
        // from one, so the first file each way is transfer 1. Keyed on the id
        // alone those two got the same key and both started at nonce zero:
        // xor the ciphertexts and both files fall out, over a plain TCP
        // connection that sits outside the Noise session.
        let hh = b"one session, two directions";
        assert_ne!(
            key(hh, A, 1),
            key(hh, B, 1),
            "the same id offered from each end must not derive the same key"
        );

        // Said the way it actually fails: a chunk sealed one way must not open
        // the other way.
        let sealed = seal(&key(hh, A, 1), 0, b"a secret file").unwrap();
        assert_eq!(
            open(&key(hh, B, 1), 0, &sealed),
            Err(ChunkError::Sealed(0)),
            "the other direction's key must open nothing"
        );
    }

    #[test]
    fn the_offerer_is_length_prefixed_so_it_cannot_run_into_the_id() {
        // Without the length byte, ("AB", 1) and ("A", …) could produce the
        // same info string from different inputs. Device ids are fixed width
        // today, which is exactly the assumption worth not depending on.
        assert_ne!(key(b"hh", "AB", 1), key(b"hh", "A", 1));
    }

    #[test]
    fn a_name_cannot_hide_dots_behind_a_control_character() {
        // The order of the two cleanups is the whole finding. Dots were trimmed
        // first, so a name starting with something invisible kept its dots
        // through the trim and lost the invisible part to the filter after —
        // handing back the one name this function must never produce.
        assert_eq!(safe_name("\u{0}.."), "received");
        assert_eq!(safe_name("\u{1}."), "received");
        assert_eq!(safe_name("\u{7}../etc/passwd"), "passwd");
        assert_eq!(safe_name("\u{0}.bashrc"), "bashrc");
        // And the ordinary case is untouched.
        assert_eq!(safe_name("holiday.jpg"), "holiday.jpg");
    }

    #[test]
    fn a_name_is_bounded_in_bytes_because_a_filesystem_is() {
        // 120 characters of emoji is 480 bytes, and a name over 255 cannot be
        // created at all: the transfer used to fail with an errno rather than
        // with a reason.
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
        // Transfer ids restart per session, so without the handshake hash in
        // the derivation a recorded transfer could be replayed into a later one.
        assert_ne!(key(b"session one", A, 1), key(b"session two", A, 1));
    }

    #[test]
    fn a_bulk_key_is_not_the_session_psk() {
        // Both derive from a handshake hash. Without the info string they would
        // be the same bytes, and a transfer key handed to a host — which is the
        // point of deriving one — would be the session's own key.
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
        // A name that rewrites the line it is printed on is a name nobody
        // should have to think about again.
        assert_eq!(safe_name("in\u{1b}[2Kvoice.pdf"), "in[2Kvoice.pdf");
        assert!(!safe_name("a\nb.txt").contains('\n'));
    }
}
