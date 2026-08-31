//! BLE fragmentation, shared by both transports: one header byte per fragment.
//!
//! ```text
//! bit 0  MORE    more fragments belong to this message
//! bit 1  START   this fragment begins a message
//! 2..7           reserved, must be zero
//! ```
//!
//! `START` is redundant on a reliable ordered link; it is kept to turn desyncs
//! into errors instead of silent corruption.

use alloc::vec::Vec;

pub const MORE: u8 = 0x01;
pub const START: u8 = 0x02;
const RESERVED: u8 = !(MORE | START);

pub const HEADER: usize = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BleError {
    Empty,
    Reserved,
    /// A continuation with no message open, or a start while one was unfinished.
    Desync,
    /// Reassembly would exceed `max_message`; reported before the bytes are kept.
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

/// Cut a message into fragments of at most `fragment` bytes each, header included.
///
/// A `fragment` of 1 has no room for payload and is treated as 2. An empty
/// message is one header-only fragment, not zero fragments.
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

/// Puts fragments back together. Owned by one link and dropped with it, so a
/// connection that dies mid-message cannot leak half of one into the next.
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
    /// See [`BleError`]. Any error leaves the reassembler closed; drop the link.
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
            self.fail();
            return Err(BleError::Desync);
        }
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
        // 23 is the ATT default, 185 the long-time iOS value, 517 the modern ceiling.
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
        // An evenly dividing message must not emit a trailing empty fragment.
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
        // `>=` would refuse exactly `max_message`, which the core's own check allows.
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
