//! The pairing QR payload.
//!
//! Specified in `docs/PROTOCOL.md` § 8 since M0 and implemented nowhere until
//! M3, which is why pairing meant reading eight characters aloud and typing an
//! IP address on a phone keyboard.
//!
//! ```text
//! acrylius:1?n=<name>&h=<host>&p=<port>&id=<device id>&fp=<fingerprint>&c=<code>
//! ```
//!
//! Here rather than in either host, because both ends need it and they must
//! agree exactly: the desktop draws it and the phone reads it. Two
//! implementations of a format is the thing this project exists to avoid.
//!
//! **Nothing in a scanned payload is trusted.** It supplies a candidate address
//! to dial and a code to derive the pairing PSK from, and that is all. The
//! fingerprint is checked against the one the handshake actually produces and a
//! mismatch aborts — so a forged QR costs an attacker a failed handshake, not a
//! pairing.

use alloc::string::String;
use alloc::vec::Vec;

use crate::ids::{DeviceId, Fingerprint};

/// What a pairing QR carries.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PairingQr {
    /// For showing. Never for deciding.
    pub name: String,
    pub host: String,
    pub port: u16,
    pub device_id: DeviceId,
    /// Checked against what the handshake produces. A mismatch aborts.
    pub fingerprint: Fingerprint,
    /// The pairing code, which is the `XXpsk0` pre-shared key.
    pub code: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QrError {
    /// Not an acrylius payload at all.
    NotOurs,
    /// Ours, from a version this build does not know.
    Version,
    /// A field is missing, repeated, or malformed.
    Malformed,
}

impl core::fmt::Display for QrError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::NotOurs => "not an acrylius pairing code",
            Self::Version => "made by a newer version of acrylius",
            Self::Malformed => "not a well-formed acrylius pairing code",
        })
    }
}

const SCHEME: &str = "acrylius:";
/// Bumped only for a change the query string cannot express. A reader that
/// meets a higher one says so rather than guessing, because guessing here means
/// dialling somewhere on a stranger's say-so.
const VERSION: &str = "1";

impl PairingQr {
    /// The payload to draw.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut s = String::from(SCHEME);
        s.push_str(VERSION);
        s.push('?');
        for (i, (k, v)) in [
            ("n", self.name.as_str()),
            ("h", self.host.as_str()),
            ("id", self.device_id.as_str()),
            ("fp", self.fingerprint.as_str()),
            ("c", self.code.as_str()),
        ]
        .iter()
        .enumerate()
        {
            if i > 0 {
                s.push('&');
            }
            s.push_str(k);
            s.push('=');
            escape_into(&mut s, v);
        }
        // Numeric, so it needs no escaping and cannot be confused for a field
        // somebody typed.
        s.push_str("&p=");
        push_u16(&mut s, self.port);
        s
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// [`QrError`] when the payload is not ours, is from a version this build
    /// does not know, or does not carry every field in a form it can use.
    pub fn decode(text: &str) -> Result<Self, QrError> {
        let rest = text.strip_prefix(SCHEME).ok_or(QrError::NotOurs)?;
        let (version, query) = rest.split_once('?').ok_or(QrError::Malformed)?;
        if version != VERSION {
            return Err(QrError::Version);
        }

        let (mut name, mut host, mut port) = (None, None, None);
        let (mut id, mut fp, mut code) = (None, None, None);
        for field in query.split('&') {
            let (k, v) = field.split_once('=').ok_or(QrError::Malformed)?;
            let v = unescape(v)?;
            // A repeated key is refused rather than resolved. Two answers to
            // one question is a payload somebody built by hand, and picking
            // either is picking one for them.
            let slot = match k {
                "n" => &mut name,
                "h" => &mut host,
                "p" => &mut port,
                "id" => &mut id,
                "fp" => &mut fp,
                "c" => &mut code,
                // Room to add a field later without stranding old readers.
                _ => continue,
            };
            if slot.is_some() {
                return Err(QrError::Malformed);
            }
            *slot = Some(v);
        }

        Ok(Self {
            name: name.ok_or(QrError::Malformed)?,
            host: host.ok_or(QrError::Malformed)?,
            port: port
                .ok_or(QrError::Malformed)?
                .parse()
                .map_err(|_| QrError::Malformed)?,
            device_id: DeviceId::parse(&id.ok_or(QrError::Malformed)?)
                .map_err(|_| QrError::Malformed)?,
            fingerprint: Fingerprint::parse(&fp.ok_or(QrError::Malformed)?)
                .map_err(|_| QrError::Malformed)?,
            code: code.ok_or(QrError::Malformed)?,
        })
    }

    /// `host:port`, which is what a dial takes.
    #[must_use]
    pub fn addr(&self) -> String {
        let mut s = self.host.clone();
        s.push(':');
        push_u16(&mut s, self.port);
        s
    }
}

fn push_u16(s: &mut String, n: u16) {
    // `alloc` has no `format!` worth pulling in here for one number.
    let mut buf = [0u8; 5];
    let mut i = buf.len();
    let mut n = n;
    loop {
        i -= 1;
        buf[i] = b'0' + u8::try_from(n % 10).unwrap_or(0);
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for b in &buf[i..] {
        s.push(char::from(*b));
    }
}

/// Percent-encode everything that is not plainly safe.
///
/// Conservative on purpose. A device name is whatever somebody typed into their
/// operating system, so it routinely contains spaces, and may contain `&` or
/// `=` — either of which would silently split the payload into fields that were
/// never meant to exist.
fn escape_into(out: &mut String, s: &str) {
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(*b));
        } else {
            out.push('%');
            out.push(hex(b >> 4));
            out.push(hex(b & 0x0f));
        }
    }
}

fn hex(nibble: u8) -> char {
    char::from(match nibble {
        0..=9 => b'0' + nibble,
        _ => b'A' + (nibble - 10),
    })
}

fn unescape(s: &str) -> Result<String, QrError> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = bytes.get(i + 1).copied().ok_or(QrError::Malformed)?;
                let lo = bytes.get(i + 2).copied().ok_or(QrError::Malformed)?;
                out.push((nibble(hi)? << 4) | nibble(lo)?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| QrError::Malformed)
}

fn nibble(b: u8) -> Result<u8, QrError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(QrError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    const ID: &str = "SVQGk97EtiSgiwoEuKDsJg";
    const FP: &str = "rZ_J9P03wHIW93Ao5Z17HzRMtSXeh58J0F7Q4DG-mA0";

    fn sample(name: &str) -> PairingQr {
        PairingQr {
            name: name.to_string(),
            host: "192.168.1.20".to_string(),
            port: 1971,
            device_id: DeviceId::parse(ID).unwrap(),
            fingerprint: Fingerprint::parse(FP).unwrap(),
            code: "K7QM3XPA".to_string(),
        }
    }

    #[test]
    fn the_payload_is_the_one_the_document_specifies() {
        // A golden vector, because the phone that reads this and the desktop
        // that draws it ship separately: a change here that looks harmless is
        // a phone that cannot scan the computer it is sitting next to.
        assert_eq!(
            sample("desktop").encode(),
            format!("acrylius:1?n=desktop&h=192.168.1.20&id={ID}&fp={FP}&c=K7QM3XPA&p=1971")
        );
    }

    #[test]
    fn it_round_trips() {
        let q = sample("desktop");
        assert_eq!(PairingQr::decode(&q.encode()), Ok(q));
    }

    #[test]
    fn a_name_with_punctuation_survives_the_trip() {
        // The case that would otherwise split the payload into fields nobody
        // meant: a device name is whatever somebody typed into their OS.
        for name in ["Vercixx's PC", "work & home", "a=b", "кухня", "100% mine"] {
            let q = sample(name);
            let text = q.encode();
            assert_eq!(
                text.matches('&').count(),
                5,
                "{name:?} escaped into extra fields: {text}"
            );
            assert_eq!(
                PairingQr::decode(&text).map(|d| d.name),
                Ok(name.to_string())
            );
        }
    }

    #[test]
    fn a_dial_takes_host_and_port_together() {
        assert_eq!(sample("pc").addr(), "192.168.1.20:1971");
    }

    #[test]
    fn something_else_entirely_is_not_ours() {
        for text in ["https://example.com", "WIFI:S:home;;", "", "acrylius"] {
            assert_eq!(PairingQr::decode(text), Err(QrError::NotOurs), "{text:?}");
        }
    }

    #[test]
    fn a_newer_version_says_so_rather_than_guessing() {
        // Guessing here means dialling somewhere on a stranger's say-so.
        let text = sample("pc").encode().replace("acrylius:1?", "acrylius:2?");
        assert_eq!(PairingQr::decode(&text), Err(QrError::Version));
    }

    #[test]
    fn a_field_that_is_missing_or_repeated_is_refused() {
        let good = sample("pc").encode();
        // Two answers to one question is a payload built by hand, and picking
        // either one is picking it for whoever built it.
        assert_eq!(
            PairingQr::decode(&format!("{good}&c=OTHER")),
            Err(QrError::Malformed)
        );
        // Every field is load-bearing; none may be inferred.
        for key in ["n=", "h=", "id=", "fp=", "c=", "p="] {
            let without: alloc::vec::Vec<&str> = good
                .split('?')
                .nth(1)
                .unwrap()
                .split('&')
                .filter(|f| !f.starts_with(key))
                .collect();
            let text = format!("acrylius:1?{}", without.join("&"));
            assert_eq!(
                PairingQr::decode(&text),
                Err(QrError::Malformed),
                "dropping {key} should be refused"
            );
        }
    }

    #[test]
    fn a_fingerprint_that_is_not_one_is_refused() {
        // It is the field the handshake is checked against, so a payload
        // carrying nonsense here must fail now rather than at the point where
        // the comparison quietly cannot be made.
        let text = sample("pc").encode().replace(FP, "not-a-fingerprint");
        assert_eq!(PairingQr::decode(&text), Err(QrError::Malformed));
    }
}
