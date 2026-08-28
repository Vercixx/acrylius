//! The BLE fragmentation codec, handed to Swift.
//!
//! Swift does not reimplement this. `acrylius-proto` owns where a message ends
//! on a BLE link, exactly as it owns where a bulk chunk ends, and a phone that
//! disagreed with the daemon by one byte would produce a link that handshakes
//! and then quietly corrupts everything after it.
//!
//! The seam stays what it is everywhere else: synchronous, one-directional, no
//! callbacks. Swift calls in; nothing calls back out.

use acrylius_proto::ble;

use crate::types::FfiError;

/// Cut a whole message into fragments that fit one write or one notification.
///
/// `fragment` is the ATT payload the link can carry — on iOS,
/// `maximumWriteValueLength(for:)`, which is the negotiated MTU minus three.
/// It is asked for, never assumed.
#[uniffi::export]
#[must_use]
pub fn ble_fragment(msg: Vec<u8>, fragment: u32) -> Vec<Vec<u8>> {
    ble::fragment(&msg, fragment as usize)
}

/// How much header every fragment carries, so a host can size its writes.
#[uniffi::export]
#[must_use]
pub fn ble_header_len() -> u32 {
    ble::HEADER as u32
}

/// Puts fragments back together for one link.
///
/// Stateful, so it is an object rather than a function: it belongs to a link and
/// is dropped with it. A connection that dies mid-message therefore cannot leak
/// half of one into the next, which is a property of the ownership rather than
/// of anyone remembering to call a reset.
#[derive(uniffi::Object)]
pub struct BleReassembler {
    inner: std::sync::Mutex<ble::Reassembler>,
}

#[uniffi::export]
impl BleReassembler {
    /// `max_message` must be the link's own `max_message`, so an oversized
    /// message is refused here rather than after it has been held in memory.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(max_message: u32) -> Self {
        Self {
            inner: std::sync::Mutex::new(ble::Reassembler::new(max_message as usize)),
        }
    }

    /// Feed one fragment. Returns the whole message when this was its last, and
    /// `None` while more are still expected.
    ///
    /// # Errors
    ///
    /// A malformed or oversized stream. The caller's answer is to drop the
    /// link: after an error this reassembler expects a fresh message and the
    /// bytes it was holding are gone.
    pub fn push(&self, fragment: Vec<u8>) -> Result<Option<Vec<u8>>, FfiError> {
        let mut inner = self.inner.lock().map_err(|_| FfiError::Effect {
            detail: "the reassembler was poisoned by an earlier panic".to_string(),
        })?;
        inner.push(&fragment).map_err(|e| FfiError::BadInput {
            detail: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_halves_agree_across_the_seam() {
        let msg: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
        let r = BleReassembler::new(1 << 16);
        let mut last = None;
        for f in ble_fragment(msg.clone(), 185) {
            last = r.push(f).expect("what we produced must reassemble");
        }
        assert_eq!(last, Some(msg));
    }

    #[test]
    fn an_oversized_message_is_refused_rather_than_held() {
        let r = BleReassembler::new(8);
        let mut err = None;
        for f in ble_fragment(vec![0u8; 64], 16) {
            if let Err(e) = r.push(f) {
                err = Some(e);
                break;
            }
        }
        assert!(matches!(err, Some(FfiError::BadInput { .. })));
    }
}
