//! Golden vectors, asserted against `docs/PROTOCOL.md`.
//!
//! There is only one implementation of this protocol, so these are not here to
//! keep two codebases in step. They are here to keep the *document* in step with
//! the code: a spec nothing checks is a spec that quietly becomes fiction, and
//! this one has to be good enough for a second implementation to be written from
//! years later.
//!
//! Every value below also appears in `docs/PROTOCOL.md`. Changing one without
//! the other fails here.

use acrylius_proto::{b64, envelope::Envelope, handshake::Hello, ids, pairing};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const ZERO: [u8; 32] = [0u8; 32];

fn one() -> [u8; 32] {
    let mut k = [0u8; 32];
    k[31] = 1;
    k
}

#[test]
fn identity_vectors() {
    assert_eq!(
        ids::Fingerprint::of(&ZERO).as_str(),
        "sHwTuvaZ9cfLpIyLPH6e4z2F8YLj2cXSbe-5M0xmQPQ"
    );
    assert_eq!(ids::DeviceId::of(&ZERO).as_str(), "gWEIONLBax9DtyHhuB1DsQ");
    assert_eq!(
        ids::Fingerprint::of(&one()).as_str(),
        "NzL-Mnw4P7v4wEgbthBDmhE15enJlMjoeD_iFFt6cwU"
    );
    assert_eq!(ids::DeviceId::of(&one()).as_str(), "MMq6cFxD0KKxxurgY8epBg");
}

#[test]
fn base64url_vectors() {
    assert_eq!(b64::encode(b"foobar"), "Zm9vYmFy");
    assert_eq!(b64::encode(b"fo"), "Zm8");
    // The two bytes that separate base64url from base64.
    assert_eq!(b64::encode(&[0xFF, 0xEF, 0xBF]), "_--_");
}

#[test]
fn pairing_vectors() {
    let norm = pairing::normalize("ABCD1234").unwrap();
    assert_eq!(norm, "ABCD1234");
    // Lower case and the confusable four fold onto the same code.
    assert_eq!(pairing::normalize("abcd1234").unwrap(), norm);
    // The four confusable characters fold: I and L onto 1, O onto 0, U onto V.
    // Separators are stripped, so a code can be written out in groups.
    assert_eq!(pairing::normalize("IOIO-UUUU").unwrap(), "1010VVVV");
    assert_eq!(pairing::normalize("l0l0 uuuu").unwrap(), "1010VVVV");

    assert_eq!(
        b64::encode(&pairing::psk(&norm)),
        "dyRc5CXtth81rAlg0fgf1GXo8Nx8JDlXuuHcLNJnWv8"
    );

    assert_eq!(pairing::encode(0), "00000000");
    assert_eq!(pairing::encode(0xFF_FFFF_FFFF), "ZZZZZZZZ");
}

#[test]
fn derivation_vectors() {
    let hh = b"acrylius test handshake hash";
    assert_eq!(pairing::sas(hh), "605 480");
    assert_eq!(
        b64::encode(&pairing::session_psk(hh)),
        "ETrFmyDTNXLF0pTa1DE49WPipihv9qMK60V2Bf6DAx0"
    );
}

#[test]
fn envelope_vector() {
    let e = Envelope::new(7, "org.acrylius.ping/1", "ping", b"hi");
    let bytes = e.encode().unwrap();
    assert_eq!(
        hex(&bytes),
        "870107f6736f72672e616372796c6975732e70696e672f316470696e6742686900"
    );
    // A CBOR array of seven, not eight: `bulk` is nil and minicbor omits a
    // trailing nil. That is the same mechanism that lets a later version append
    // field 8 without breaking this reader.
    assert_eq!(bytes[0], 0x87);
    assert_eq!(bytes.len(), 33);
    assert_eq!(Envelope::decode(&bytes).unwrap(), e);
}

#[test]
fn hello_vector() {
    let h = Hello {
        v: acrylius_proto::WIRE_VERSION,
        ts_ms: 1_700_000_000_000,
        device_id: "AAAAAAAAAAAAAAAAAAAAAA".into(),
        name: "pc".into(),
        platform: "linux".into(),
        caps_out: vec!["org.acrylius.ping/1".into()],
        caps_in: vec!["org.acrylius.ping/1".into()],
    };
    let bytes = minicbor::to_vec(&h).unwrap();
    assert_eq!(
        hex(&bytes),
        "87011b0000018bcfe568007641414141414141414141414141414141414141414141\
         627063656c696e757881736f72672e616372796c6975732e70696e672f3181736f7267\
         2e616372796c6975732e70696e672f31"
            .replace(['\n', ' '], "")
    );
    assert_eq!(minicbor::decode::<Hello>(&bytes).unwrap(), h);
}
