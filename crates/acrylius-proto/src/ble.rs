//! Fragmentation for a BLE link: a header byte, and nothing else.
//!
//! A GATT characteristic carries an ATT payload of a couple of hundred bytes,
//! and section 5 says a transport delivers whole messages. Something has to cut
//! a message up and put it back together, and that something is here rather than
//! in a transport for the same reason the bulk sealing is
//! (see [`crate::bulk`]): there are two transports. The daemon moves fragments
//! over zbus to BlueZ; a phone moves them over CoreBluetooth. If each carried
//! its own idea of where a message ends there would be two implementations of
//! the wire format, which is the single thing this project exists to avoid.
//!
//! So the format lives here as plain buffer transforms — no socket, no runtime,
//! no async — and each transport only decides how bytes reach the wire.
//!
//! ## The header
//!
//! One byte in front of every fragment:
//!
//! ```text
//! bit 0  MORE    more fragments belong to this message
//! bit 1  START   this fragment begins a message
//! 2..7           reserved, must be zero
//! ```
//!
//! So a message that fits one fragment is `0x02`, and a message in three is
//! `0x03`, `0x01`, `0x00`.
//!
//! `START` is redundant on a link that never loses or reorders, which is what
//! `LinkAttrs::reliable && ordered` promises. It is here anyway because it costs
//! no bytes and turns two silent failures — a continuation arriving with no
//! message open, and a new message beginning while one is still unfinished —
//! into errors that name themselves.

use alloc::vec::Vec;

/// More fragments belong to this message.
pub const MORE: u8 = 0x01;
/// This fragment begins a message.
pub const START: u8 = 0x02;
/// Bits nothing may set yet. A peer that sets one is speaking a dialect we do
/// not have, and guessing at it is worse than saying so.
const RESERVED: u8 = !(MORE | START);

/// Every fragment carries this much header.
pub const HEADER: usize = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BleError {
    /// A fragment with no header byte at all.
    Empty,
    /// A reserved header bit was set.
    Reserved,
    /// A continuation arrived with no message open, or a message began while
    /// one was still unfinished.
    Desync,
    /// Reassembling would exceed the link's `max_message`. Reported before the
    /// bytes are kept, so a peer cannot make us hold what it never has to send.
    TooLarge,
}

impl core::fmt::Display for BleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::Empty => "a fragment with no header",
            Self::Reserved => "a reserved header bit was set",
            Self::Desync => "a fragment that does not continue anything",
            Self::TooLarge => "the message is larger than the link allows",
        };
        f.write_str(s)
    }
}

/// Cut a whole message into fragments of at most `fragment` bytes each,
/// **header included**.
///
/// `fragment` is what the link can carry in one write or one notification — on
/// BlueZ that is the `mtu` the daemon is handed rather than a number anyone
/// guessed. A `fragment` of 1 leaves no room for payload and would never
/// terminate, so it is treated as 2.
///
/// An empty message is one fragment carrying only a header, not zero fragments:
/// a message that exists and says nothing still has to arrive.
#[must_use]
pub fn fragment(msg: &[u8], fragment: usize) -> Vec<Vec<u8>> {
    let payload = fragment.saturating_sub(HEADER).max(1);
    let mut out = Vec::new();
    let mut rest = msg;
    let mut first = true;
    loop {
        let take = rest.len().min(payload);
        let (now, later) = rest.split_at(take);
        rest = later;
        let last = rest.is_empty();
        let mut frag = Vec::with_capacity(HEADER + now.len());
        frag.push(if first { START } else { 0 } | if last { 0 } else { MORE });
        frag.extend_from_slice(now);
        out.push(frag);
        first = false;
        if last {
            break;
        }
    }
    out
}

/// Puts fragments back together.
///
/// Owned by one link and dropped with it, so a connection that dies mid-message
/// cannot leak half of one into the next.
pub struct Reassembler {
    buf: Vec<u8>,
    max: usize,
    open: bool,
}

impl Reassembler {
    #[must_use]
    pub fn new(max_message: usize) -> Self {
        Self {
            buf: Vec::new(),
            max: max_message,
            open: false,
        }
    }

    /// Feed one fragment. Returns the whole message when this was its last.
    ///
    /// # Errors
    ///
    /// See [`BleError`]. Any error leaves the reassembler closed: the stream is
    /// no longer trustworthy, and the caller's job is to drop the link rather
    /// than to carry on and hope.
    pub fn push(&mut self, frag: &[u8]) -> Result<Option<Vec<u8>>, BleError> {
        let Some((&header, body)) = frag.split_first() else {
            self.fail();
            return Err(BleError::Empty);
        };
        if header & RESERVED != 0 {
            self.fail();
            return Err(BleError::Reserved);
        }
        let starts = header & START != 0;
        if starts == self.open {
            // Either a message began while one was unfinished, or a
            // continuation arrived with nothing to continue.
            self.fail();
            return Err(BleError::Desync);
        }
        // Checked before the bytes are kept, which is the same rule the TCP
        // transport follows for its length prefix.
        if self.buf.len().saturating_add(body.len()) > self.max {
            self.fail();
            return Err(BleError::TooLarge);
        }
        self.buf.extend_from_slice(body);
        if header & MORE != 0 {
            self.open = true;
            return Ok(None);
        }
        self.open = false;
        Ok(Some(core::mem::take(&mut self.buf)))
    }

    fn fail(&mut self) {
        self.buf.clear();
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn round_trip(msg: &[u8], mtu: usize) -> Vec<u8> {
        let mut r = Reassembler::new(1 << 20);
        let mut last = None;
        for f in fragment(msg, mtu) {
            assert!(f.len() <= mtu.max(2), "a fragment must fit the link");
            last = r.push(&f).expect("a fragment we produced must reassemble");
        }
        last.expect("the final fragment completes the message")
    }

    #[test]
    fn a_message_survives_every_plausible_mtu() {
        let msg: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        // 23 is the ATT default, 185 what iOS long negotiated, 517 the modern
        // ceiling. The exact numbers matter less than that none of them change
        // what arrives.
        for mtu in [23, 27, 64, 185, 247, 517, 1024] {
            assert_eq!(round_trip(&msg, mtu), msg, "mtu {mtu}");
        }
    }

    #[test]
    fn a_message_that_fits_is_one_fragment() {
        let f = fragment(b"hi", 185);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0][0], START, "begins and ends here, so MORE is clear");
    }

    #[test]
    fn an_empty_message_still_arrives() {
        assert_eq!(round_trip(b"", 185), b"");
    }

    #[test]
    fn the_headers_say_what_they_should() {
        // 4 bytes per fragment, one of them header: 3 bytes of payload each.
        let headers = |msg: &[u8]| -> Vec<u8> { fragment(msg, 4).iter().map(|x| x[0]).collect() };
        assert_eq!(headers(&[0u8; 10]), vec![START | MORE, MORE, MORE, 0]);
        // A message that divides evenly must not emit a trailing empty
        // fragment just to carry the closing header.
        assert_eq!(headers(&[0u8; 9]), vec![START | MORE, MORE, 0]);
    }

    #[test]
    fn a_continuation_with_nothing_to_continue_is_refused() {
        let mut r = Reassembler::new(1 << 20);
        assert_eq!(r.push(&[MORE, 1, 2, 3]), Err(BleError::Desync));
    }

    #[test]
    fn a_message_that_begins_while_one_is_open_is_refused() {
        let mut r = Reassembler::new(1 << 20);
        assert_eq!(r.push(&[START | MORE, 1]), Ok(None));
        assert_eq!(r.push(&[START | MORE, 2]), Err(BleError::Desync));
    }

    #[test]
    fn a_message_larger_than_the_link_allows_is_refused_before_it_is_kept() {
        let mut r = Reassembler::new(4);
        assert_eq!(r.push(&[START | MORE, 1, 2, 3]), Ok(None));
        assert_eq!(r.push(&[0, 4, 5]), Err(BleError::TooLarge));
    }

    #[test]
    fn a_message_of_exactly_the_size_allowed_is_kept() {
        // The boundary the `>` guards, and which nothing pinned: a `>=` here
        // refuses a message of exactly `max_message`, which the other end is
        // entitled to send. The core's own check is `len > max_message`, so the
        // two would disagree by one byte and the link would be dropped for a
        // message that was never too big.
        let mut r = Reassembler::new(4);
        assert_eq!(r.push(&[START | MORE, 1, 2]), Ok(None));
        assert_eq!(r.push(&[0, 3, 4]), Ok(Some(vec![1, 2, 3, 4])));
    }

    #[test]
    fn a_fragment_with_no_header_is_refused() {
        let mut r = Reassembler::new(16);
        assert_eq!(r.push(&[]), Err(BleError::Empty));
    }

    #[test]
    fn a_reserved_bit_is_refused_rather_than_ignored() {
        let mut r = Reassembler::new(16);
        assert_eq!(r.push(&[START | 0x80]), Err(BleError::Reserved));
    }

    #[test]
    fn an_error_closes_the_stream_rather_than_leaving_half_a_message() {
        let mut r = Reassembler::new(1 << 20);
        assert_eq!(r.push(&[START | MORE, 1, 2]), Ok(None));
        assert_eq!(r.push(&[START, 9]), Err(BleError::Desync));
        // Whatever comes next must begin a message; the two stray bytes are gone.
        assert_eq!(r.push(&[START, 7]), Ok(Some(vec![7])));
    }
}
