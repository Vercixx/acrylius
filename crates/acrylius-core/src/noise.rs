//! The Noise layer: two patterns, one prologue, no IO.
//!
//! `snow` is itself sans-IO. `write_message(payload, &mut out)` and
//! `read_message(msg, &mut out)` are pure buffer transforms, so this module
//! composes with the core's state machine without a shim, and every test below
//! runs with no sockets and no clock.
//!
//! ## Two patterns, and why each
//!
//! Pairing is `XXpsk0`. Plain `XX` would authenticate nothing until a human
//! compared a short string carefully, and humans do not. We already have an
//! out-of-band channel, the code `acryliusctl pair` prints, so the code is
//! mixed in as a pre-shared key. `XX` also means neither side needs to know the
//! other's static key in advance, which is exactly the situation on first
//! contact.
//!
//! The PSK goes at position 0, not 3, and the difference is not cosmetic.
//! `psk3` mixes the key into message 3, which the initiator writes, so it
//! proves to the responder that the initiator knew the code and proves nothing
//! in the other direction. An initiator talking to the wrong machine would
//! complete its side, display a short authentication string, and sit waiting for
//! a human to approve a peer that had never demonstrated knowing anything.
//! `psk0` mixes the code into the chaining key before the first message, so
//! every encrypted payload in either direction depends on it: a wrong code means
//! message 1 does not decrypt, no reply is ever sent, and no code is displayed
//! anywhere. The loopback suite pins this as
//! `a_wrong_pairing_code_does_not_pair`.
//!
//! Sessions are `IKpsk2`. The initiator already knows the responder's static
//! key from pairing, so the session is up in one round trip, which matters
//! because an App Intent gets seconds of process life. The PSK is derived at
//! pairing time and never transmitted, so a session opener stays opaque even to
//! someone who has somehow obtained the responder's static key.
//!
//! ## The prologue
//!
//! Both patterns are run with a prologue naming the wire version and the mode.
//! Noise mixes the prologue into the handshake hash, so it is authenticated
//! without being secret: a MITM flipping `Session` to `Pair`, to force a device
//! back into a pairing it did not ask for, causes a decrypt failure on both
//! sides rather than a downgrade.

use snow::{Builder, HandshakeState, TransportState};

use crate::link::LinkAttrs;

/// Noise's own hard limit on a single message.
pub const MAX_NOISE_MESSAGE: usize = 65535;

const PAIR_PARAMS: &str = "Noise_XXpsk0_25519_ChaChaPoly_SHA256";
const SESSION_PARAMS: &str = "Noise_IKpsk2_25519_ChaChaPoly_SHA256";

/// PSK positions, from the pattern names above. Getting these wrong is a silent
/// interop failure rather than a compile error, so they are named once here.
const PAIR_PSK_INDEX: u8 = 0;
const SESSION_PSK_INDEX: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// First contact. Neither side knows the other's static key yet.
    Pair,
    /// A known peer. The initiator knows the responder's static key.
    Session,
}

impl Mode {
    fn params(self) -> &'static str {
        match self {
            Self::Pair => PAIR_PARAMS,
            Self::Session => SESSION_PARAMS,
        }
    }

    fn psk_index(self) -> u8 {
        match self {
            Self::Pair => PAIR_PSK_INDEX,
            Self::Session => SESSION_PSK_INDEX,
        }
    }

    /// The prologue, mixed into the handshake hash by both sides.
    ///
    /// Not secret, since an observer sees it, but authenticated, so it cannot
    /// be altered in flight without both sides noticing.
    fn prologue(self) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(b"ACR");
        p.push(crate::proto::WIRE_VERSION);
        p.push(match self {
            Self::Pair => 1,
            Self::Session => 2,
        });
        let params = self.params().as_bytes();
        p.push(u8::try_from(params.len()).expect("pattern names are short"));
        p.extend_from_slice(params);
        p
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    #[error("noise: {0}")]
    Snow(#[from] snow::Error),
    #[error("message of {got} bytes exceeds the Noise limit of {MAX_NOISE_MESSAGE}")]
    TooLarge { got: usize },
    #[error("the handshake is already complete")]
    AlreadyComplete,
    #[error("the handshake is not complete yet")]
    NotComplete,
    #[error("this link is lossy or unordered, which needs a stateless cipher")]
    UnsupportedLink,
}

/// A static X25519 keypair. The device's long-term identity.
#[derive(Clone)]
pub struct Identity {
    private: [u8; 32],
    public: [u8; 32],
}

impl Identity {
    /// Generate a fresh identity. Takes the pattern only to reuse `snow`'s
    /// resolver; the key is not pattern-specific.
    pub fn generate() -> Result<Self, NoiseError> {
        let kp =
            Builder::new(PAIR_PARAMS.parse().expect("static pattern parses")).generate_keypair()?;
        Ok(Self {
            private: kp
                .private
                .as_slice()
                .try_into()
                .expect("x25519 private key is 32 bytes"),
            public: kp
                .public
                .as_slice()
                .try_into()
                .expect("x25519 public key is 32 bytes"),
        })
    }

    /// Load an identity from its stored private half.
    ///
    /// The public key is derived, never stored alongside and trusted: a file
    /// that had been edited to pair a real private key with someone else's
    /// public key would otherwise produce an identity whose fingerprint lies
    /// about which key it can actually prove possession of.
    #[must_use]
    pub fn from_private(private: [u8; 32]) -> Self {
        let secret = x25519_dalek::StaticSecret::from(private);
        let public = x25519_dalek::PublicKey::from(&secret);
        Self {
            private,
            public: public.to_bytes(),
        }
    }

    #[must_use]
    pub fn public(&self) -> &[u8; 32] {
        &self.public
    }

    #[must_use]
    pub fn private(&self) -> &[u8; 32] {
        &self.private
    }

    #[must_use]
    pub fn fingerprint(&self) -> crate::proto::ids::Fingerprint {
        crate::proto::ids::Fingerprint::of(&self.public)
    }

    #[must_use]
    pub fn device_id(&self) -> crate::proto::ids::DeviceId {
        crate::proto::ids::DeviceId::of(&self.public)
    }
}

impl core::fmt::Debug for Identity {
    /// Never print the private half, not even in a panic message.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

/// A handshake in progress.
pub struct Handshake {
    state: HandshakeState,
    mode: Mode,
}

impl Handshake {
    /// Start a pairing handshake. `psk` comes from the pairing code, and with
    /// `psk0` it is required before either side can read anything at all.
    pub fn pair_initiator(id: &Identity, psk: &[u8; 32]) -> Result<Self, NoiseError> {
        Self::build(Mode::Pair, id, psk, None, true)
    }

    pub fn pair_responder(id: &Identity, psk: &[u8; 32]) -> Result<Self, NoiseError> {
        Self::build(Mode::Pair, id, psk, None, false)
    }

    /// Start a session handshake with an already-paired peer.
    pub fn session_initiator(
        id: &Identity,
        psk: &[u8; 32],
        peer_static: &[u8; 32],
    ) -> Result<Self, NoiseError> {
        Self::build(Mode::Session, id, psk, Some(peer_static), true)
    }

    pub fn session_responder(id: &Identity, psk: &[u8; 32]) -> Result<Self, NoiseError> {
        Self::build(Mode::Session, id, psk, None, false)
    }

    /// Learn who is calling, before we know which PSK to answer with.
    ///
    /// `IKpsk2` puts the responder in an awkward spot: `snow` wants the PSK at
    /// build time, but the PSK is per-peer and the peer's identity arrives
    /// inside message 1. The way out is that `psk2` is mixed at message 2:
    /// message 1's tokens are `e, es, s, ss` and its payload is encrypted under a
    /// chaining key the PSK has not touched yet. So a throwaway handshake built
    /// with a zero PSK reads message 1 to exactly the same result as the real one
    /// would.
    ///
    /// The caller uses the returned static key to find the peer, then builds a
    /// real responder with that peer's PSK and replays the same message 1 into
    /// it. Nothing is leaked and nothing is trusted: the identity learned here is
    /// only used to choose a PSK, and if the choice is wrong, message 2 fails
    /// on the initiator exactly as it should.
    pub fn session_identify(id: &Identity, msg1: &[u8]) -> Result<[u8; 32], NoiseError> {
        let mut probe = Self::build(Mode::Session, id, &[0u8; 32], None, false)?;
        probe.read(msg1)?;
        probe.peer_static().ok_or(NoiseError::NotComplete)
    }

    fn build(
        mode: Mode,
        id: &Identity,
        psk: &[u8; 32],
        peer_static: Option<&[u8; 32]>,
        initiator: bool,
    ) -> Result<Self, NoiseError> {
        let prologue = mode.prologue();
        let mut b = Builder::new(mode.params().parse().expect("static pattern parses"))
            .prologue(&prologue)?
            .local_private_key(id.private())?
            .psk(mode.psk_index(), psk)?;
        if let Some(rs) = peer_static {
            b = b.remote_public_key(rs)?;
        }
        let state = if initiator {
            b.build_initiator()?
        } else {
            b.build_responder()?
        };
        Ok(Self { state, mode })
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// Whether it is our turn to write.
    #[must_use]
    pub fn is_my_turn(&self) -> bool {
        !self.is_complete() && self.state.is_my_turn()
    }

    /// Produce the next handshake message, carrying `payload`.
    pub fn write(&mut self, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if self.is_complete() {
            return Err(NoiseError::AlreadyComplete);
        }
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = self.state.write_message(payload, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume a handshake message, returning its payload.
    pub fn read(&mut self, msg: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if msg.len() > MAX_NOISE_MESSAGE {
            return Err(NoiseError::TooLarge { got: msg.len() });
        }
        if self.is_complete() {
            return Err(NoiseError::AlreadyComplete);
        }
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = self.state.read_message(msg, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// The peer's static public key, once the pattern has transmitted it.
    ///
    /// `None` early in `XX`, which is exactly why a pairing decision cannot be
    /// made before the handshake completes.
    #[must_use]
    pub fn peer_static(&self) -> Option<[u8; 32]> {
        self.state.get_remote_static()?.try_into().ok()
    }

    /// The handshake hash, for the SAS and for deriving the session PSK.
    ///
    /// Only meaningful once complete. Before that it is a running value, and two
    /// honest peers would derive different strings from it.
    pub fn handshake_hash(&self) -> Result<Vec<u8>, NoiseError> {
        if !self.is_complete() {
            return Err(NoiseError::NotComplete);
        }
        Ok(self.state.get_handshake_hash().to_vec())
    }

    /// Move into transport mode.
    pub fn into_session(self, attrs: &LinkAttrs) -> Result<Session, NoiseError> {
        if !self.is_complete() {
            return Err(NoiseError::NotComplete);
        }
        if !attrs.supports_stateful_cipher() {
            // A lossy or unordered link needs caller-supplied nonces plus a
            // replay window. The plumbing to notice that is here; the stateless
            // path itself lands with the first such transport.
            return Err(NoiseError::UnsupportedLink);
        }
        Ok(Session {
            state: self.state.into_transport_mode()?,
            sent: 0,
        })
    }
}

/// An established session.
pub struct Session {
    state: TransportState,
    sent: u64,
}

/// Rekey before the cipher's nonce space gets anywhere near exhaustion.
/// ChaChaPoly's counter is 64-bit, so this is enormously conservative, which is
/// the point: it costs nothing and removes a class of bug entirely.
pub const REKEY_AFTER_MESSAGES: u64 = 1 << 20;

impl Session {
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = self.state.write_message(plaintext, &mut buf)?;
        buf.truncate(n);
        self.sent += 1;
        Ok(buf)
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if ciphertext.len() > MAX_NOISE_MESSAGE {
            return Err(NoiseError::TooLarge {
                got: ciphertext.len(),
            });
        }
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = self.state.read_message(ciphertext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    #[must_use]
    pub fn should_rekey(&self) -> bool {
        self.sent >= REKEY_AFTER_MESSAGES
    }

    pub fn rekey_outgoing(&mut self) {
        self.state.rekey_outgoing();
        self.sent = 0;
    }
}

impl core::fmt::Debug for Session {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Session")
            .field("sent", &self.sent)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::TransportId;
    use crate::proto::pairing;

    fn loopback() -> LinkAttrs {
        LinkAttrs::loopback(TransportId(0))
    }

    /// Drive a full XXpsk3 pairing to completion, returning both halves.
    fn pair(
        a: &Identity,
        b: &Identity,
        psk_a: &[u8; 32],
        psk_b: &[u8; 32],
    ) -> Result<(Handshake, Handshake), NoiseError> {
        let mut i = Handshake::pair_initiator(a, psk_a)?;
        let mut r = Handshake::pair_responder(b, psk_b)?;
        let m1 = i.write(b"")?;
        r.read(&m1)?;
        let m2 = r.write(b"responder hello")?;
        i.read(&m2)?;
        let m3 = i.write(b"initiator hello")?;
        r.read(&m3)?;
        Ok((i, r))
    }

    fn session(
        init: &Identity,
        resp: &Identity,
        psk: &[u8; 32],
    ) -> Result<(Handshake, Handshake), NoiseError> {
        let mut i = Handshake::session_initiator(init, psk, resp.public())?;
        let mut r = Handshake::session_responder(resp, psk)?;
        let m1 = i.write(b"hello")?;
        r.read(&m1)?;
        let m2 = r.write(b"hello back")?;
        i.read(&m2)?;
        Ok((i, r))
    }

    #[test]
    fn identity_derives_a_stable_public_half() {
        let id = Identity::generate().unwrap();
        let reloaded = Identity::from_private(*id.private());
        assert_eq!(id.public(), reloaded.public());
        assert_eq!(id.fingerprint(), reloaded.fingerprint());
    }

    #[test]
    fn identity_debug_never_leaks_the_private_half() {
        let id = Identity::generate().unwrap();
        let rendered = format!("{id:?}");
        let secret = crate::proto::b64::encode(id.private());
        assert!(
            !rendered.contains(&secret),
            "Debug must not print the private key"
        );
    }

    #[test]
    fn pairing_completes_and_both_sides_agree_on_the_hash() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let psk = pairing::psk(&pairing::normalize("ABCD1234").unwrap());
        let (i, r) = pair(&a, &b, &psk, &psk).unwrap();

        assert!(i.is_complete() && r.is_complete());
        assert_eq!(i.handshake_hash().unwrap(), r.handshake_hash().unwrap());
        // Each side learned the other's real static key.
        assert_eq!(i.peer_static().unwrap(), *b.public());
        assert_eq!(r.peer_static().unwrap(), *a.public());
    }

    #[test]
    fn both_ends_display_the_same_sas() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let psk = pairing::psk(&pairing::normalize("ABCD1234").unwrap());
        let (i, r) = pair(&a, &b, &psk, &psk).unwrap();
        // The property the whole pairing screen rests on.
        assert_eq!(
            pairing::sas(&i.handshake_hash().unwrap()),
            pairing::sas(&r.handshake_hash().unwrap())
        );
    }

    #[test]
    fn a_wrong_pairing_code_fails_at_the_very_first_message() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let right = pairing::psk(&pairing::normalize("ABCD1234").unwrap());
        let wrong = pairing::psk(&pairing::normalize("ABCD1235").unwrap());
        // Not "a check returns false": the handshake cannot start.
        assert!(pair(&a, &b, &right, &wrong).is_err());

        // Specifically: the responder cannot even read message 1, so it never
        // replies and the initiator never reaches a state where it would show a
        // code. With psk3 this would have failed three messages later, after the
        // initiator had already completed and displayed a SAS.
        let mut i = Handshake::pair_initiator(&a, &right).unwrap();
        let mut r = Handshake::pair_responder(&b, &wrong).unwrap();
        let m1 = i.write(b"").unwrap();
        assert!(
            r.read(&m1).is_err(),
            "message 1 must already depend on the code"
        );
        assert!(!i.is_complete());
    }

    #[test]
    fn a_pairing_peer_is_unknown_until_the_pattern_transmits_it() {
        // This is why a pairing decision cannot be made early: for most of XX
        // there is simply nobody identified to decide about.
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let psk = pairing::psk(&pairing::normalize("ABCD1234").unwrap());
        let mut i = Handshake::pair_initiator(&a, &psk).unwrap();
        let mut r = Handshake::pair_responder(&b, &psk).unwrap();

        let m1 = i.write(b"").unwrap();
        r.read(&m1).unwrap();
        assert_eq!(
            r.peer_static(),
            None,
            "responder must not know the initiator yet"
        );
        assert_eq!(
            i.peer_static(),
            None,
            "initiator must not know the responder yet"
        );

        let m2 = r.write(b"").unwrap();
        i.read(&m2).unwrap();
        assert_eq!(i.peer_static().unwrap(), *b.public());
        assert_eq!(r.peer_static(), None, "still nothing for the responder");

        let m3 = i.write(b"").unwrap();
        r.read(&m3).unwrap();
        assert_eq!(r.peer_static().unwrap(), *a.public());
    }

    #[test]
    fn the_handshake_hash_is_refused_before_completion() {
        let a = Identity::generate().unwrap();
        let psk = [0u8; 32];
        let hs = Handshake::pair_initiator(&a, &psk).unwrap();
        assert!(matches!(hs.handshake_hash(), Err(NoiseError::NotComplete)));
    }

    #[test]
    fn session_completes_in_one_round_trip() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let psk = [7u8; 32];
        let (i, r) = session(&a, &b, &psk).unwrap();
        assert!(i.is_complete() && r.is_complete());
        assert_eq!(i.handshake_hash().unwrap(), r.handshake_hash().unwrap());
        assert_eq!(i.peer_static().unwrap(), *b.public());
        assert_eq!(r.peer_static().unwrap(), *a.public());
    }

    #[test]
    fn a_session_needs_the_right_peer_static_key() {
        // Dialling the right address but the wrong machine must not connect.
        let (a, b, impostor) = (
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
            Identity::generate().unwrap(),
        );
        let psk = [7u8; 32];
        let mut i = Handshake::session_initiator(&a, &psk, impostor.public()).unwrap();
        let mut r = Handshake::session_responder(&b, &psk).unwrap();
        let m1 = i.write(b"hello").unwrap();
        assert!(
            r.read(&m1).is_err(),
            "b must not accept a handshake aimed at another key"
        );
    }

    #[test]
    fn a_session_needs_the_right_psk() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let mut i = Handshake::session_initiator(&a, &[1u8; 32], b.public()).unwrap();
        let mut r = Handshake::session_responder(&b, &[2u8; 32]).unwrap();
        let m1 = i.write(b"hello").unwrap();
        // IKpsk2 mixes the PSK at message 2, so msg1 reads fine and msg2 is
        // where an impostor is caught.
        r.read(&m1).unwrap();
        let m2 = r.write(b"hello back").unwrap();
        assert!(
            i.read(&m2).is_err(),
            "a wrong session PSK must not complete"
        );
    }

    #[test]
    fn the_prologue_separates_the_two_modes() {
        let p = Mode::Pair.prologue();
        let s = Mode::Session.prologue();
        assert_ne!(p, s);
        assert!(p.starts_with(b"ACR"));
        assert_eq!(p[3], crate::proto::WIRE_VERSION, "version is authenticated");
        assert_ne!(p[4], s[4], "the mode byte is what a downgrade would flip");
    }

    #[test]
    fn a_pairing_handshake_cannot_be_answered_as_a_session() {
        // The downgrade a MITM would attempt: push a paired device back into
        // pairing. Different patterns AND a different prologue both refuse it.
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let psk = [3u8; 32];
        let mut i = Handshake::pair_initiator(&a, &psk).unwrap();
        let mut r = Handshake::session_responder(&b, &psk).unwrap();
        let m1 = i.write(b"").unwrap();
        assert!(r.read(&m1).is_err());
    }

    #[test]
    fn a_session_carries_messages_both_ways() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let psk = [7u8; 32];
        let (i, r) = session(&a, &b, &psk).unwrap();
        let mut si = i.into_session(&loopback()).unwrap();
        let mut sr = r.into_session(&loopback()).unwrap();

        let ct = si.encrypt(b"lock the session").unwrap();
        assert_eq!(sr.decrypt(&ct).unwrap(), b"lock the session");

        let ct = sr.encrypt(b"locked").unwrap();
        assert_eq!(si.decrypt(&ct).unwrap(), b"locked");
    }

    #[test]
    fn a_tampered_ciphertext_is_refused() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (i, r) = session(&a, &b, &[7u8; 32]).unwrap();
        let mut si = i.into_session(&loopback()).unwrap();
        let mut sr = r.into_session(&loopback()).unwrap();

        let mut ct = si.encrypt(b"unlock").unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(sr.decrypt(&ct).is_err());
    }

    #[test]
    fn a_replayed_session_message_is_refused() {
        // This is the property that deletes the old project's SQLite nonce
        // table: the cipher's own counter refuses a replay, for free.
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (i, r) = session(&a, &b, &[7u8; 32]).unwrap();
        let mut si = i.into_session(&loopback()).unwrap();
        let mut sr = r.into_session(&loopback()).unwrap();

        let ct = si.encrypt(b"unlock").unwrap();
        assert_eq!(sr.decrypt(&ct).unwrap(), b"unlock");
        assert!(
            sr.decrypt(&ct).is_err(),
            "the same ciphertext must not decrypt twice"
        );
    }

    #[test]
    fn messages_are_not_interchangeable_between_sessions() {
        // Two independent pairs of peers. A message from one must be worthless
        // against the other, which is the cross-server replay the old protocol
        // needed a SERVER_FP term in every signature to prevent.
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (c, d) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (i1, _r1) = session(&a, &b, &[7u8; 32]).unwrap();
        let (_i2, r2) = session(&c, &d, &[7u8; 32]).unwrap();
        let mut s1 = i1.into_session(&loopback()).unwrap();
        let mut s2 = r2.into_session(&loopback()).unwrap();

        let ct = s1.encrypt(b"unlock").unwrap();
        assert!(s2.decrypt(&ct).is_err());
    }

    #[test]
    fn an_incomplete_handshake_cannot_become_a_session() {
        let a = Identity::generate().unwrap();
        let hs = Handshake::pair_initiator(&a, &[0u8; 32]).unwrap();
        assert!(matches!(
            hs.into_session(&loopback()),
            Err(NoiseError::NotComplete)
        ));
    }

    #[test]
    fn a_lossy_link_is_refused_a_stateful_session() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (i, _r) = session(&a, &b, &[7u8; 32]).unwrap();
        let mut attrs = loopback();
        attrs.ordered = false;
        assert!(matches!(
            i.into_session(&attrs),
            Err(NoiseError::UnsupportedLink)
        ));
    }

    #[test]
    fn oversized_input_is_rejected_before_snow_sees_it() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (i, _r) = session(&a, &b, &[7u8; 32]).unwrap();
        let mut s = i.into_session(&loopback()).unwrap();
        let huge = vec![0u8; MAX_NOISE_MESSAGE + 1];
        assert!(matches!(s.decrypt(&huge), Err(NoiseError::TooLarge { .. })));
    }
}

#[cfg(test)]
mod identify_tests {
    use super::*;

    #[test]
    fn identify_learns_the_caller_and_the_real_handshake_still_completes() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let psk = [9u8; 32];

        let mut i = Handshake::session_initiator(&a, &psk, b.public()).unwrap();
        let m1 = i.write(b"hello").unwrap();

        // The responder has no idea who this is yet.
        let who = Handshake::session_identify(&b, &m1).unwrap();
        assert_eq!(who, *a.public());

        // Having chosen a PSK from that, replay the very same message 1.
        let mut r = Handshake::session_responder(&b, &psk).unwrap();
        assert_eq!(r.read(&m1).unwrap(), b"hello");
        let m2 = r.write(b"hi").unwrap();
        i.read(&m2).unwrap();
        assert!(i.is_complete() && r.is_complete());
        assert_eq!(i.handshake_hash().unwrap(), r.handshake_hash().unwrap());
    }

    #[test]
    fn identifying_does_not_let_a_wrong_psk_slip_through() {
        // The identity learned by probing chooses a PSK; it must not also
        // *authorise* anything. Choosing the wrong one still has to fail.
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let mut i = Handshake::session_initiator(&a, &[9u8; 32], b.public()).unwrap();
        let m1 = i.write(b"hello").unwrap();

        assert_eq!(Handshake::session_identify(&b, &m1).unwrap(), *a.public());

        let mut r = Handshake::session_responder(&b, &[8u8; 32]).unwrap();
        r.read(&m1).unwrap();
        let m2 = r.write(b"hi").unwrap();
        assert!(
            i.read(&m2).is_err(),
            "a mischosen PSK must still fail at message 2"
        );
    }
}
