//! Touchpad over USB: the phone listens on loopback, this end reaches it through
//! usbmuxd's socket, says hello, then replays touches.

use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use acrylius_core::plugins::touchpad::{Begin, Frame, MAX_POINTS};
use acrylius_core::vocab::TouchPoint;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::touchpad::Device;

/// The port the phone listens on.
pub const PORT: u16 = 1972;

const USBMUXD: &str = "/run/usbmuxd";

/// usbmuxd's binary protocol (version 0), which needs no plist parser.
const MUX_HEADER: u32 = 16;
const MUX_RESULT: u32 = 1;
const MUX_CONNECT: u32 = 2;
const MUX_LISTEN: u32 = 3;
const MUX_ATTACHED: u32 = 4;
const MUX_DETACHED: u32 = 5;

const MAX_MESSAGE: u32 = 4096;

const NO_DEVICE: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(1);

/// The phone repeats a held frame every 100 ms, so this much silence with
/// fingers down means it was suspended or unplugged mid-drag.
const STALL: Duration = Duration::from_millis(300);

/// Covers one batch of samples: iOS delivers a 120 Hz digitizer's samples in
/// pairs once per 60 Hz frame.
const PLAYOUT: Duration = Duration::from_millis(10);

#[derive(Debug, PartialEq, Eq)]
enum Msg {
    Begin {
        w_mm: u16,
        h_mm: u16,
    },
    Frame {
        points: Vec<TouchPoint>,
        t_us: Option<u32>,
    },
    End,
}

/// Replays a batch that arrived at once at the spacing it was sampled at,
/// `PLAYOUT` behind the quickest sample seen so far.
#[derive(Default)]
struct Pacer {
    anchor: Option<(Instant, u32)>,
}

impl Pacer {
    fn due(&mut self, t_us: u32, arrived: Instant) -> Instant {
        let predicted = self
            .anchor
            .map(|(at, t0)| at + Duration::from_micros(u64::from(t_us.wrapping_sub(t0))));
        let predicted = match predicted {
            Some(p) if p <= arrived => p,
            _ => {
                self.anchor = Some((arrived, t_us));
                arrived
            }
        };
        (predicted + PLAYOUT).max(arrived)
    }
}

fn decode(msg: &[u8]) -> Result<Msg, &'static str> {
    let (&kind, body) = msg.split_first().ok_or("empty message")?;
    match kind {
        0 => {
            let b: Begin = minicbor::decode(body).map_err(|_| "malformed begin")?;
            if b.w_mm == 0 || b.h_mm == 0 {
                return Err("zero-sized surface");
            }
            Ok(Msg::Begin {
                w_mm: b.w_mm,
                h_mm: b.h_mm,
            })
        }
        1 => {
            let f: Frame = minicbor::decode(body).map_err(|_| "malformed frame")?;
            if f.points.len() > MAX_POINTS {
                return Err("more points than slots");
            }
            Ok(Msg::Frame {
                points: f
                    .points
                    .into_iter()
                    .map(|p| TouchPoint {
                        id: p.id,
                        x: p.x,
                        y: p.y,
                    })
                    .collect(),
                t_us: f.t_us,
            })
        }
        2 => Ok(Msg::End),
        _ => Err("unknown message kind"),
    }
}

/// Owns the read half so the future can sit in a `select!` across iterations
/// and hand it back when done.
async fn read_message(mut rd: OwnedReadHalf) -> (OwnedReadHalf, std::io::Result<Vec<u8>>) {
    let n = match rd.read_u32().await {
        Ok(n) => n,
        Err(e) => return (rd, Err(e)),
    };
    if n > MAX_MESSAGE {
        let e = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message of {n} bytes exceeds the {MAX_MESSAGE} cap"),
        );
        return (rd, Err(e));
    }
    let mut buf = vec![0u8; n as usize];
    let read = rd.read_exact(&mut buf).await.map(|_| buf);
    (rd, read)
}

trait Pad {
    fn apply(&mut self, points: &[TouchPoint]) -> std::io::Result<()>;
    fn release_all(&mut self) -> std::io::Result<()>;
}

impl Pad for Device {
    fn apply(&mut self, points: &[TouchPoint]) -> std::io::Result<()> {
        Device::apply(self, points)
    }

    fn release_all(&mut self) -> std::io::Result<()> {
        Device::release_all(self)
    }
}

/// Runs until the phone goes away. `heard` is set once the phone has spoken,
/// so a connection that never got going is not logged.
async fn session<P: Pad>(
    stream: UnixStream,
    device_id: &str,
    heard: &mut bool,
    open: impl Fn(u16, u16) -> std::io::Result<P>,
) -> std::io::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let len = u32::try_from(device_id.len()).map_err(std::io::Error::other)?;
    let mut hello = len.to_be_bytes().to_vec();
    hello.extend_from_slice(device_id.as_bytes());
    wr.write_all(&hello).await?;

    let mut reading = Box::pin(read_message(rd));
    let mut queue: VecDeque<(Instant, Msg)> = VecDeque::new();
    let mut pacer = Pacer::default();
    let mut queued_down = false;
    let mut device: Option<((u16, u16), P)> = None;
    let mut down = false;
    let mut last_heard = Instant::now();
    loop {
        let now = Instant::now();
        while queue.front().is_some_and(|(due, _)| *due <= now) {
            if let Some((_, msg)) = queue.pop_front() {
                apply(msg, &mut device, &mut down, &open)?;
            }
        }
        let next_due = queue.front().map(|(due, _)| *due);
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(next_due.unwrap_or(now)), if next_due.is_some() => {}
            (rd, read) = &mut reading => {
                let arrived = Instant::now();
                let msg = decode(&read?)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                if !*heard {
                    *heard = true;
                    tracing::info!("phone touchpad connected over USB");
                }
                last_heard = arrived;
                let due = match &msg {
                    Msg::Frame { points, t_us } => {
                        // A new gesture re-anchors, so clock drift never builds up.
                        if !queued_down {
                            pacer = Pacer::default();
                        }
                        queued_down = !points.is_empty();
                        t_us.map_or(arrived, |t| pacer.due(t, arrived))
                    }
                    Msg::Begin { .. } => arrived,
                    Msg::End => {
                        queued_down = false;
                        arrived
                    }
                };
                queue.push_back((due, msg));
                reading.set(read_message(rd));
            }
            () = tokio::time::sleep_until(last_heard + STALL), if down && queue.is_empty() => {
                down = false;
                if let Some((_, d)) = device.as_mut() {
                    d.release_all()?;
                }
            }
        }
    }
}

fn apply<P: Pad>(
    msg: Msg,
    device: &mut Option<((u16, u16), P)>,
    down: &mut bool,
    open: impl Fn(u16, u16) -> std::io::Result<P>,
) -> std::io::Result<()> {
    match msg {
        Msg::Begin { w_mm, h_mm } => {
            if device
                .as_ref()
                .is_none_or(|(dims, _)| *dims != (w_mm, h_mm))
            {
                *device = Some(((w_mm, h_mm), open(w_mm, h_mm)?));
            }
        }
        Msg::Frame { points, .. } => {
            *down = !points.is_empty();
            if let Some((_, d)) = device.as_mut() {
                d.apply(&points)?;
            }
        }
        Msg::End => {
            *down = false;
            *device = None;
        }
    }
    Ok(())
}

// ponytail: any local user can also reach the phone's port through usbmuxd, and
// the phone serves the last hello. Sign the hello with the pairing keys if that matters.
pub async fn drive_touchpad(device_id: String) {
    let mux = Path::new(USBMUXD);
    let (tx, mut rx) = watch::channel(None);
    tokio::spawn(watch_devices(mux, tx));
    loop {
        let Ok(Some(dev)) = rx.wait_for(Option::is_some).await.map(|d| *d) else {
            return;
        };
        if let Ok(stream) = mux_open(mux, MUX_CONNECT, &connect_body(dev, PORT)).await {
            let mut heard = false;
            if let Err(e) = session(stream, &device_id, &mut heard, Device::create).await
                && heard
            {
                tracing::info!(error = %e, "phone touchpad disconnected");
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Publishes the usbmuxd id of the first attached device, `None` while there is none.
async fn watch_devices(mux: &Path, tx: watch::Sender<Option<u32>>) {
    loop {
        if let Ok(mut s) = mux_open(mux, MUX_LISTEN, &[]).await {
            let mut attached = Vec::new();
            while let Ok((kind, body)) = mux_read(&mut s).await {
                let Some(id) = body.first_chunk().map(|b| u32::from_le_bytes(*b)) else {
                    continue;
                };
                match kind {
                    MUX_ATTACHED => attached.push(id),
                    MUX_DETACHED => attached.retain(|d| *d != id),
                    _ => continue,
                }
                tx.send_replace(attached.first().copied());
            }
        }
        tx.send_replace(None);
        tokio::time::sleep(NO_DEVICE).await;
    }
}

fn connect_body(device: u32, port: u16) -> [u8; 8] {
    let mut body = [0; 8];
    body[..4].copy_from_slice(&device.to_le_bytes());
    body[4..6].copy_from_slice(&port.to_be_bytes());
    body
}

/// Sends one request and returns the stream once usbmuxd answers 0. After
/// `MUX_CONNECT` the same stream carries the phone's bytes.
async fn mux_open(mux: &Path, kind: u32, body: &[u8]) -> std::io::Result<UnixStream> {
    let mut s = UnixStream::connect(mux).await?;
    let len = u32::try_from(body.len()).map_err(std::io::Error::other)? + MUX_HEADER;
    let mut msg = Vec::new();
    for word in [len, 0, kind, 1] {
        msg.extend_from_slice(&word.to_le_bytes());
    }
    msg.extend_from_slice(body);
    s.write_all(&msg).await?;
    match mux_read(&mut s).await? {
        (MUX_RESULT, code) if code == [0; 4] => Ok(s),
        (kind, body) => Err(std::io::Error::other(format!(
            "usbmuxd answered {kind} {body:?}"
        ))),
    }
}

async fn mux_read(s: &mut UnixStream) -> std::io::Result<(u32, Vec<u8>)> {
    let len = s.read_u32_le().await?;
    let _version = s.read_u32_le().await?;
    let kind = s.read_u32_le().await?;
    let _tag = s.read_u32_le().await?;
    let n = len
        .checked_sub(MUX_HEADER)
        .filter(|n| *n <= MAX_MESSAGE)
        .ok_or_else(|| std::io::Error::other(format!("usbmuxd message of {len} bytes")))?;
    let mut body = vec![0; n as usize];
    s.read_exact(&mut body).await?;
    Ok((kind, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use acrylius_core::plugins::touchpad::Point;

    fn message(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut m = vec![kind];
        m.extend_from_slice(body);
        m
    }

    fn timed(n: u8, t_us: Option<u32>) -> Vec<u8> {
        let points = (0..n).map(|id| Point { id, x: 1, y: 2 }).collect();
        let f = Frame {
            seq: 1,
            points,
            t_us,
        };
        message(1, &minicbor::to_vec(f).unwrap())
    }

    fn frame(n: u8) -> Vec<u8> {
        timed(n, None)
    }

    #[test]
    fn begin_carries_the_surface_size() {
        let body = minicbor::to_vec(Begin {
            w_mm: 70,
            h_mm: 150,
        })
        .unwrap();
        assert_eq!(
            decode(&message(0, &body)),
            Ok(Msg::Begin {
                w_mm: 70,
                h_mm: 150
            })
        );
    }

    #[test]
    fn a_zero_sized_surface_is_refused() {
        for (w_mm, h_mm) in [(0, 5), (5, 0)] {
            let body = minicbor::to_vec(Begin { w_mm, h_mm }).unwrap();
            assert!(decode(&message(0, &body)).is_err());
        }
    }

    #[test]
    fn a_frame_carries_its_points() {
        assert_eq!(
            decode(&timed(2, Some(7))),
            Ok(Msg::Frame {
                points: vec![
                    TouchPoint { id: 0, x: 1, y: 2 },
                    TouchPoint { id: 1, x: 1, y: 2 },
                ],
                t_us: Some(7),
            })
        );
    }

    #[test]
    fn a_batch_leaves_at_the_spacing_it_was_sampled_at() {
        let mut p = Pacer::default();
        let at = Instant::now();
        let ms = Duration::from_millis;
        // Two 120 Hz samples per 60 Hz delivery; the first batch only anchors.
        assert_eq!(p.due(0, at), at + PLAYOUT);
        assert_eq!(p.due(8_000, at), at + PLAYOUT);
        let next = at + ms(16);
        assert_eq!(p.due(16_000, next), at + ms(8) + PLAYOUT);
        assert_eq!(p.due(24_000, next), at + ms(16) + PLAYOUT);
    }

    #[test]
    fn a_late_sample_goes_out_at_once_and_an_early_one_reanchors() {
        let mut p = Pacer::default();
        let at = Instant::now();
        let ms = Duration::from_millis;
        p.due(0, at);
        assert_eq!(p.due(10_000, at + ms(100)), at + ms(100));
        assert_eq!(p.due(20_000, at + ms(5)), at + ms(5) + PLAYOUT);
    }

    #[test]
    fn sample_time_wraps() {
        let mut p = Pacer::default();
        let at = Instant::now();
        p.due(u32::MAX - 999, at);
        let later = at + Duration::from_millis(5);
        assert_eq!(p.due(1_000, later), at + Duration::from_millis(2) + PLAYOUT);
    }

    #[test]
    fn exactly_max_points_is_accepted_one_more_is_refused() {
        assert!(decode(&frame(MAX_POINTS as u8)).is_ok());
        assert!(decode(&frame(MAX_POINTS as u8 + 1)).is_err());
    }

    #[test]
    fn end_unknown_kinds_and_empty_messages() {
        assert_eq!(decode(&[2]), Ok(Msg::End));
        assert!(decode(&[3]).is_err());
        assert!(decode(&[]).is_err());
        assert!(decode(&[0, 0xff]).is_err());
    }

    type Log = std::sync::Arc<std::sync::Mutex<Vec<(String, Instant)>>>;

    fn note(log: &Log, line: String) {
        log.lock().unwrap().push((line, Instant::now()));
    }

    fn when(log: &Log, line: &str) -> Option<Instant> {
        log.lock()
            .unwrap()
            .iter()
            .find(|(l, _)| l == line)
            .map(|(_, at)| *at)
    }

    struct Fake {
        log: Log,
        dims: (u16, u16),
    }

    impl Fake {
        fn note(&self, line: String) {
            note(&self.log, line);
        }
    }

    impl Pad for Fake {
        fn apply(&mut self, points: &[TouchPoint]) -> std::io::Result<()> {
            self.note(format!("apply {}", points.len()));
            Ok(())
        }

        fn release_all(&mut self) -> std::io::Result<()> {
            self.note("release".to_string());
            Ok(())
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.note(format!("drop {}x{}", self.dims.0, self.dims.1));
        }
    }

    /// The phone's end of a session against a fake pad, after hello was checked.
    async fn start() -> (
        UnixStream,
        Log,
        tokio::task::JoinHandle<(std::io::Result<()>, bool)>,
    ) {
        let (desktop, mut phone) = UnixStream::pair().unwrap();
        let log = Log::default();
        let opened = log.clone();
        let task = tokio::spawn(async move {
            let mut heard = false;
            let r = session(desktop, "dev", &mut heard, |w, h| {
                note(&opened, format!("open {w}x{h}"));
                Ok(Fake {
                    log: opened.clone(),
                    dims: (w, h),
                })
            })
            .await;
            (r, heard)
        });
        let n = phone.read_u32().await.unwrap();
        let mut hello = vec![0; n as usize];
        phone.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello, b"dev");
        (phone, log, task)
    }

    async fn say(phone: &mut UnixStream, msg: &[u8]) {
        phone.write_u32(msg.len() as u32).await.unwrap();
        phone.write_all(msg).await.unwrap();
    }

    fn begin(w_mm: u16, h_mm: u16) -> Vec<u8> {
        message(0, &minicbor::to_vec(Begin { w_mm, h_mm }).unwrap())
    }

    async fn finish(
        phone: UnixStream,
        log: Log,
        task: tokio::task::JoinHandle<(std::io::Result<()>, bool)>,
    ) -> (std::io::ErrorKind, bool, Vec<String>) {
        drop(phone);
        let (r, heard) = task.await.unwrap();
        let lines = log.lock().unwrap().iter().map(|(l, _)| l.clone()).collect();
        (r.unwrap_err().kind(), heard, lines)
    }

    #[tokio::test]
    async fn a_timed_frame_waits_out_the_playout_and_an_untimed_one_does_not() {
        let (mut phone, log, task) = start().await;
        say(&mut phone, &begin(70, 150)).await;
        let sent = Instant::now();
        say(&mut phone, &timed(1, Some(0))).await;
        tokio::time::sleep(PLAYOUT * 3).await;
        let applied = when(&log, "apply 1").expect("the timed frame was applied");
        assert!(applied >= sent + PLAYOUT);

        let sent = Instant::now();
        say(&mut phone, &frame(2)).await;
        tokio::time::sleep(PLAYOUT * 3).await;
        let applied = when(&log, "apply 2").expect("the untimed frame was applied");
        assert!(applied < sent + PLAYOUT);
        finish(phone, log, task).await;
    }

    #[tokio::test]
    async fn a_repeated_begin_keeps_the_device_and_a_new_size_replaces_it() {
        let (mut phone, log, task) = start().await;
        say(&mut phone, &begin(70, 150)).await;
        say(&mut phone, &begin(70, 150)).await;
        say(&mut phone, &frame(1)).await;
        say(&mut phone, &begin(80, 150)).await;
        say(&mut phone, &[2]).await;
        let (kind, heard, lines) = finish(phone, log, task).await;
        assert_eq!(kind, std::io::ErrorKind::UnexpectedEof);
        assert!(heard);
        assert_eq!(
            lines,
            [
                "open 70x150",
                "apply 1",
                "open 80x150",
                "drop 70x150",
                "drop 80x150"
            ]
        );
    }

    #[tokio::test]
    async fn silence_lifts_fingers_only_while_some_are_down() {
        let (mut phone, log, task) = start().await;
        say(&mut phone, &begin(70, 150)).await;
        say(&mut phone, &frame(0)).await;
        tokio::time::sleep(STALL * 2).await;
        say(&mut phone, &frame(1)).await;
        tokio::time::sleep(STALL * 2).await;
        let (_, _, lines) = finish(phone, log, task).await;
        assert_eq!(
            lines,
            [
                "open 70x150",
                "apply 0",
                "apply 1",
                "release",
                "drop 70x150"
            ]
        );
    }

    #[tokio::test]
    async fn a_message_at_the_cap_is_read_and_one_past_it_ends_the_session() {
        let (mut phone, log, task) = start().await;
        let mut at_cap = vec![0u8; MAX_MESSAGE as usize];
        at_cap[0] = 2;
        say(&mut phone, &at_cap).await;
        say(&mut phone, &begin(70, 150)).await;
        phone.write_u32(MAX_MESSAGE + 1).await.unwrap();
        let (kind, _, lines) = finish(phone, log, task).await;
        assert_eq!(kind, std::io::ErrorKind::InvalidData);
        assert_eq!(lines, ["open 70x150", "drop 70x150"]);
    }

    fn mux_message(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        for word in [MUX_HEADER + body.len() as u32, 0, kind, 0] {
            m.extend_from_slice(&word.to_le_bytes());
        }
        m.extend_from_slice(body);
        m
    }

    fn fake_mux(name: &str) -> (std::path::PathBuf, tokio::net::UnixListener) {
        let path = std::env::temp_dir().join(format!("acrylius-mux-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        (path, listener)
    }

    async fn answer_connect(mux: tokio::net::UnixListener, code: u32) -> (UnixStream, [u8; 24]) {
        let (mut s, _) = mux.accept().await.unwrap();
        let mut req = [0; 24];
        s.read_exact(&mut req).await.unwrap();
        s.write_all(&mux_message(MUX_RESULT, &code.to_le_bytes()))
            .await
            .unwrap();
        (s, req)
    }

    #[tokio::test]
    async fn connect_names_the_device_and_port_then_hands_over_the_stream() {
        let (path, mux) = fake_mux("connect");
        let server = tokio::spawn(async move {
            let (mut s, req) = answer_connect(mux, 0).await;
            s.write_all(b"phone").await.unwrap();
            req
        });
        let mut s = mux_open(&path, MUX_CONNECT, &connect_body(3, PORT))
            .await
            .unwrap();
        let mut rest = [0; 5];
        s.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"phone");
        assert_eq!(
            server.await.unwrap(),
            [
                24, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 0x07, 0xb4, 0, 0
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_connect_is_an_error() {
        let (path, mux) = fake_mux("refused");
        tokio::spawn(answer_connect(mux, 3));
        assert!(
            mux_open(&path, MUX_CONNECT, &connect_body(3, PORT))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_mux_length_outside_header_to_cap_is_refused() {
        for (len, ok) in [
            (MUX_HEADER - 1, false),
            (MUX_HEADER + MAX_MESSAGE, true),
            (MUX_HEADER + MAX_MESSAGE + 1, false),
        ] {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            let mut m = mux_message(MUX_RESULT, &vec![0; MAX_MESSAGE as usize]);
            m[..4].copy_from_slice(&len.to_le_bytes());
            a.write_all(&m).await.unwrap();
            drop(a);
            match mux_read(&mut b).await {
                Ok(_) => assert!(ok),
                Err(e) => assert!(!ok && e.kind() == std::io::ErrorKind::Other),
            }
        }
    }

    async fn next(rx: &mut watch::Receiver<Option<u32>>) -> Option<u32> {
        tokio::time::timeout(Duration::from_secs(1), rx.changed())
            .await
            .unwrap()
            .unwrap();
        *rx.borrow_and_update()
    }

    #[tokio::test]
    async fn the_first_attached_device_is_published_until_usbmuxd_goes_away() {
        let (path, mux) = fake_mux("listen");
        let (tx, mut rx) = watch::channel(None);
        tokio::spawn(async move { watch_devices(&path, tx).await });
        let (mut s, _) = mux.accept().await.unwrap();
        let mut req = [0; 16];
        s.read_exact(&mut req).await.unwrap();
        assert_eq!(req[8..12], MUX_LISTEN.to_le_bytes());
        s.write_all(&mux_message(MUX_RESULT, &[0; 4]))
            .await
            .unwrap();

        let record = |id: u32| {
            let mut r = vec![0; 268];
            r[..4].copy_from_slice(&id.to_le_bytes());
            r
        };
        for (kind, body, want) in [
            (MUX_ATTACHED, record(5), Some(5)),
            (MUX_ATTACHED, record(7), Some(5)),
            (MUX_DETACHED, 5u32.to_le_bytes().to_vec(), Some(7)),
        ] {
            s.write_all(&mux_message(kind, &body)).await.unwrap();
            assert_eq!(next(&mut rx).await, want);
        }
        drop(s);
        assert_eq!(next(&mut rx).await, None);
    }

    #[tokio::test]
    async fn a_phone_that_never_speaks_is_not_heard() {
        let (phone, log, task) = start().await;
        let (_, heard, lines) = finish(phone, log, task).await;
        assert!(!heard);
        assert!(lines.is_empty());
    }
}
