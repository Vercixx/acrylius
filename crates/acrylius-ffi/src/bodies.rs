//! Decoding plugin bodies, for hosts.
//!
//! The core keeps message bodies opaque, which is right for routing and useless
//! for a user interface. Rather than have Swift learn CBOR and grow a second
//! definition of every body shape, the decoders live here and hand back plain
//! records.
//!
//! This is the same argument as the rest of the project: the previous one ended
//! up with five implementations of its protocol because each surface parsed the
//! wire for itself. A view that wants a command list gets a `[FfiCommand]`.

use acrylius_core::plugins::{clipboard, command, media, session, share, wol};

use crate::FfiError;

fn bad(what: &str) -> FfiError {
    FfiError::BadInput {
        detail: format!("this is not a {what}"),
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiSessionState {
    pub locked: bool,
    pub session_id: String,
    /// `wayland` or `x11`.
    pub kind: String,
    pub active: bool,
}

#[uniffi::export]
pub fn decode_session_state(body: Vec<u8>) -> Result<FfiSessionState, FfiError> {
    let s: session::SessionState = minicbor::decode(&body).map_err(|_| bad("session state"))?;
    Ok(FfiSessionState {
        locked: s.locked,
        session_id: s.session_id,
        kind: s.kind,
        active: s.active,
    })
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiSessionOutcome {
    pub was_locked: bool,
    /// Read back after the operation, never inferred from an exit status.
    pub locked: bool,
    pub session_id: String,
}

#[uniffi::export]
pub fn decode_session_outcome(body: Vec<u8>) -> Result<FfiSessionOutcome, FfiError> {
    let o: session::SessionOutcome = minicbor::decode(&body).map_err(|_| bad("session outcome"))?;
    Ok(FfiSessionOutcome {
        was_locked: o.was_locked,
        locked: o.locked,
        session_id: o.session_id,
    })
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiCommand {
    pub id: String,
    pub name: String,
    /// A hint for the interface. It is not enforcement; the peer's allowlist is.
    pub needs_confirm: bool,
}

#[uniffi::export]
pub fn decode_command_list(body: Vec<u8>) -> Result<Vec<FfiCommand>, FfiError> {
    let l: command::CommandList = minicbor::decode(&body).map_err(|_| bad("command list"))?;
    Ok(l.commands
        .into_iter()
        .map(|c| FfiCommand {
            id: c.id,
            name: c.name,
            needs_confirm: c.needs_confirm,
        })
        .collect())
}

/// The body of a `run`. An id from the peer's own list, never a command string.
#[uniffi::export]
pub fn encode_run_request(id: String) -> Vec<u8> {
    minicbor::to_vec(command::RunRequest { id }).unwrap_or_default()
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiExited {
    pub run_id: u32,
    pub code: i32,
    pub truncated: bool,
}

#[uniffi::export]
pub fn decode_exited(body: Vec<u8>) -> Result<FfiExited, FfiError> {
    let e: command::Exited = minicbor::decode(&body).map_err(|_| bad("command outcome"))?;
    Ok(FfiExited {
        run_id: e.run_id,
        code: e.code,
        truncated: e.truncated,
    })
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiClipboard {
    pub mime: String,
    pub text: String,
}

#[uniffi::export]
pub fn decode_clipboard(body: Vec<u8>) -> Result<FfiClipboard, FfiError> {
    let c: clipboard::ClipboardSet = minicbor::decode(&body).map_err(|_| bad("clipboard value"))?;
    Ok(FfiClipboard {
        mime: c.mime,
        text: String::from_utf8_lossy(&c.data).into_owned(),
    })
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiWolConfig {
    pub macs: Vec<String>,
    pub broadcast: String,
    pub port: u16,
    /// Aim here first.
    ///
    /// A network interface matches a magic packet by its payload and ignores
    /// the destination address, so unicast wakes a machine just as well as
    /// broadcast. iOS cannot broadcast without an entitlement a free developer
    /// account cannot get, which makes this the primary path rather than a
    /// fallback. It needs the router to still hold an ARP entry for the
    /// sleeping machine.
    pub last_ipv4: String,
}

#[uniffi::export]
pub fn decode_wol_config(body: Vec<u8>) -> Result<FfiWolConfig, FfiError> {
    let c: wol::WolConfig = minicbor::decode(&body).map_err(|_| bad("wake configuration"))?;
    Ok(FfiWolConfig {
        macs: c.macs,
        broadcast: c.broadcast,
        port: c.port,
        last_ipv4: c.last_ipv4,
    })
}

/// One player, for a screen to show.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiMediaPlayer {
    pub id: String,
    pub name: String,
    pub status: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length_ms: u64,
    pub position_ms: u64,
    pub volume_percent: Option<u8>,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
    pub can_control: bool,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiMediaState {
    pub players: Vec<FfiMediaPlayer>,
    /// Which one a command with no player named goes to.
    pub active: String,
    /// The machine's own output volume, 0 to 100. `None` where there is no
    /// mixer, which is every phone: iOS gives an app no way to set the system
    /// volume, and no way to read it that is not deprecated.
    pub system_volume: Option<u8>,
}

#[uniffi::export]
pub fn decode_media_state(body: Vec<u8>) -> Result<FfiMediaState, FfiError> {
    let s: media::MediaState = minicbor::decode(&body).map_err(|_| bad("media state"))?;
    Ok(FfiMediaState {
        players: s
            .players
            .into_iter()
            .map(|p| FfiMediaPlayer {
                id: p.id,
                name: p.name,
                status: p.status,
                title: p.title,
                artist: p.artist,
                album: p.album,
                length_ms: p.length_ms,
                position_ms: p.position_ms,
                volume_percent: p.volume_percent,
                can_go_next: p.can_go_next,
                can_go_previous: p.can_go_previous,
                can_seek: p.can_seek,
                can_control: p.can_control,
            })
            .collect(),
        active: s.active,
        system_volume: s.system_volume,
    })
}

/// The body of a media command.
///
/// `player` empty means whichever is active; `value` is milliseconds for a seek
/// or position and a whole percent for a volume. Encoded here so a screen never
/// has to know the wire shape.
#[uniffi::export]
#[must_use]
pub fn encode_media_command(player: String, value: i64) -> Vec<u8> {
    minicbor::to_vec(media::MediaCommand { player, value }).unwrap_or_default()
}

#[uniffi::export]
#[must_use]
pub fn cap_media() -> String {
    media::CAP.to_string()
}

/// The bytes a host sends to wake a machine.
///
/// Built here rather than in Swift so there is one definition of the packet.
/// `ff` six times, then the MAC sixteen times: 102 bytes.
#[uniffi::export]
pub fn magic_packet(mac: String) -> Result<Vec<u8>, FfiError> {
    let hex: Vec<u8> = mac
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| u8::try_from(c.to_digit(16).unwrap_or(0)).unwrap_or(0))
        .collect();
    if hex.len() != 12 {
        return Err(FfiError::BadInput {
            detail: format!("{mac:?} is not a MAC address"),
        });
    }
    let mut addr = [0u8; 6];
    for (i, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
        addr[i] = (pair[0] << 4) | pair[1];
    }
    let mut packet = Vec::with_capacity(102);
    packet.extend_from_slice(&[0xFF; 6]);
    for _ in 0..16 {
        packet.extend_from_slice(&addr);
    }
    Ok(packet)
}

// The encoders below are the other half of the pair. A phone never sends a
// session state or a command catalogue, but a host that serves those verbs does,
// and if one is ever written in Swift it must not reimplement the format to do
// it. They are also what lets the Swift tests build a body without knowing CBOR.

#[uniffi::export]
#[must_use]
pub fn encode_session_state(state: FfiSessionState) -> Vec<u8> {
    minicbor::to_vec(session::SessionState {
        locked: state.locked,
        session_id: state.session_id,
        kind: state.kind,
        active: state.active,
    })
    .unwrap_or_default()
}

/// An offer of a file, as this device would send one.
///
/// A name, a size and an id. Never a path: where the file sits on the device
/// that owns it is not the peer's business and does not appear on the wire.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiOffer {
    pub transfer: u64,
    pub name: String,
    pub size: u64,
    pub mime: String,
}

#[uniffi::export]
#[must_use]
pub fn encode_share_offer(offer: FfiOffer) -> Vec<u8> {
    minicbor::to_vec(share::Offer {
        transfer: offer.transfer,
        name: offer.name,
        size: offer.size,
        mime: offer.mime,
    })
    .unwrap_or_default()
}

#[uniffi::export]
pub fn decode_share_offer(body: Vec<u8>) -> Result<FfiOffer, FfiError> {
    let o: share::Offer = minicbor::decode(&body).map_err(|_| bad("share offer"))?;
    Ok(FfiOffer {
        transfer: o.transfer,
        name: o.name,
        size: o.size,
        mime: o.mime,
    })
}

/// How a transfer ended, from either end.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiTransferEnd {
    pub transfer: u64,
    pub ok: bool,
    pub detail: String,
}

#[uniffi::export]
pub fn decode_share_finished(body: Vec<u8>) -> Result<FfiTransferEnd, FfiError> {
    let f: share::Finished = minicbor::decode(&body).map_err(|_| bad("share result"))?;
    Ok(FfiTransferEnd {
        transfer: f.transfer,
        ok: f.ok,
        detail: f.detail,
    })
}

#[uniffi::export]
#[must_use]
pub fn encode_media_state(state: FfiMediaState) -> Vec<u8> {
    minicbor::to_vec(media::MediaState {
        players: state
            .players
            .into_iter()
            .map(|p| media::MediaPlayer {
                id: p.id,
                name: p.name,
                status: p.status,
                title: p.title,
                artist: p.artist,
                album: p.album,
                length_ms: p.length_ms,
                position_ms: p.position_ms,
                volume_percent: p.volume_percent,
                can_go_next: p.can_go_next,
                can_go_previous: p.can_go_previous,
                can_seek: p.can_seek,
                can_control: p.can_control,
            })
            .collect(),
        active: state.active,
        system_volume: state.system_volume,
    })
    .unwrap_or_default()
}

#[uniffi::export]
#[must_use]
pub fn encode_command_list(commands: Vec<FfiCommand>) -> Vec<u8> {
    minicbor::to_vec(command::CommandList {
        commands: commands
            .into_iter()
            .map(|c| command::CommandEntry {
                id: c.id,
                name: c.name,
                needs_confirm: c.needs_confirm,
            })
            .collect(),
    })
    .unwrap_or_default()
}

#[uniffi::export]
#[must_use]
pub fn encode_clipboard(text: String) -> Vec<u8> {
    let data = text.into_bytes();
    minicbor::to_vec(clipboard::ClipboardSet {
        mime: clipboard::TEXT_PLAIN.to_string(),
        hash: clipboard::hash(&data),
        data,
    })
    .unwrap_or_default()
}

#[uniffi::export]
#[must_use]
pub fn encode_wol_config(config: FfiWolConfig) -> Vec<u8> {
    minicbor::to_vec(wol::WolConfig {
        macs: config.macs,
        broadcast: config.broadcast,
        port: config.port,
        last_ipv4: config.last_ipv4,
    })
    .unwrap_or_default()
}

/// The error code from an `err` reply, for a host that wants to say why.
#[uniffi::export]
pub fn decode_error(body: Vec<u8>) -> Result<String, FfiError> {
    let e: acrylius_proto::envelope::ErrorBody =
        minicbor::decode(&body).map_err(|_| bad("error"))?;
    Ok(if e.message.is_empty() {
        e.code
    } else {
        format!("{} ({})", e.message, e.code)
    })
}

/// Capability identifiers, so a host does not spell one wrong.
#[uniffi::export]
#[must_use]
pub fn cap_session() -> String {
    session::CAP.to_string()
}

#[uniffi::export]
#[must_use]
pub fn cap_clipboard() -> String {
    clipboard::CAP.to_string()
}

#[uniffi::export]
#[must_use]
pub fn cap_command() -> String {
    command::CAP.to_string()
}

#[uniffi::export]
#[must_use]
pub fn cap_wol() -> String {
    wol::CAP.to_string()
}

#[uniffi::export]
#[must_use]
pub fn cap_share() -> String {
    share::CAP.to_string()
}

#[uniffi::export]
#[must_use]
pub fn cap_ping() -> String {
    acrylius_core::plugins::ping::CAP.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_magic_packet_is_the_shape_the_hardware_looks_for() {
        let p = magic_packet("00:11:22:33:44:55".to_string()).unwrap();
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert_eq!(&p[6..12], &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(&p[96..102], &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    }

    #[test]
    fn a_mac_can_be_written_three_ways() {
        let a = magic_packet("aa:bb:cc:dd:ee:ff".to_string()).unwrap();
        assert_eq!(magic_packet("AA-BB-CC-DD-EE-FF".to_string()).unwrap(), a);
        assert_eq!(magic_packet("aabbccddeeff".to_string()).unwrap(), a);
    }

    #[test]
    fn anything_that_is_not_a_mac_is_refused() {
        assert!(magic_packet("nope".to_string()).is_err());
        assert!(magic_packet(String::new()).is_err());
    }

    #[test]
    fn bodies_round_trip_through_the_decoders() {
        let list = minicbor::to_vec(command::CommandList {
            commands: vec![command::CommandEntry {
                id: "screenshot".to_string(),
                name: "Screenshot".to_string(),
                needs_confirm: true,
            }],
        })
        .unwrap();
        let decoded = decode_command_list(list).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, "screenshot");
        assert!(decoded[0].needs_confirm);
    }

    #[test]
    fn every_decoder_has_a_matching_encoder() {
        let state = FfiSessionState {
            locked: true,
            session_id: "2".to_string(),
            kind: "wayland".to_string(),
            active: true,
        };
        let back = decode_session_state(encode_session_state(state.clone())).unwrap();
        assert_eq!(
            (back.locked, back.session_id, back.kind),
            (true, "2".to_string(), "wayland".to_string())
        );

        let clip = decode_clipboard(encode_clipboard("hello".to_string())).unwrap();
        assert_eq!(clip.text, "hello");

        let wake = FfiWolConfig {
            macs: vec!["00:11:22:33:44:55".to_string()],
            broadcast: "192.168.1.255".to_string(),
            port: 9,
            last_ipv4: "192.168.1.50".to_string(),
        };
        let back = decode_wol_config(encode_wol_config(wake)).unwrap();
        assert_eq!(back.last_ipv4, "192.168.1.50");
    }

    #[test]
    fn a_body_of_the_wrong_shape_is_refused_rather_than_guessed_at() {
        assert!(decode_session_state(b"not cbor".to_vec()).is_err());
        assert!(decode_command_list(vec![0xff, 0xff]).is_err());
    }
}
