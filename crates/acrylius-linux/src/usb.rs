//! Touchpad over USB: the phone listens on loopback, `iproxy` forwards a local
//! port to it, and this end connects, says hello, then replays touches.

use std::process::Stdio;
use std::time::Duration;

use acrylius_core::plugins::touchpad::{Begin, Frame, MAX_POINTS};
use acrylius_core::vocab::TouchPoint;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::touchpad::Device;

/// The port `iproxy` forwards, both sides of the tunnel.
pub const PORT: u16 = 1972;

const MAX_MESSAGE: u32 = 4096;

const NO_DEVICE: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(1);

/// The phone sends every display refresh while touched, so this much silence
/// with fingers down means it was suspended or unplugged mid-drag.
const STALL: Duration = Duration::from_millis(300);

#[derive(Debug, PartialEq, Eq)]
enum Msg {
    Begin { w_mm: u16, h_mm: u16 },
    Frame(Vec<TouchPoint>),
    End,
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
            Ok(Msg::Frame(
                f.points
                    .into_iter()
                    .map(|p| TouchPoint {
                        id: p.id,
                        x: p.x,
                        y: p.y,
                    })
                    .collect(),
            ))
        }
        2 => Ok(Msg::End),
        _ => Err("unknown message kind"),
    }
}

async fn read_message(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let n = stream.read_u32().await?;
    if n > MAX_MESSAGE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("message of {n} bytes exceeds the {MAX_MESSAGE} cap"),
        ));
    }
    let mut buf = vec![0u8; n as usize];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
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
/// since iproxy accepts the connection even when nothing listens on the phone.
async fn session<P: Pad>(
    mut stream: TcpStream,
    device_id: &str,
    heard: &mut bool,
    open: impl Fn(u16, u16) -> std::io::Result<P>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let len = u32::try_from(device_id.len()).map_err(std::io::Error::other)?;
    let mut hello = len.to_be_bytes().to_vec();
    hello.extend_from_slice(device_id.as_bytes());
    stream.write_all(&hello).await?;

    let mut device: Option<((u16, u16), P)> = None;
    let mut down = false;
    loop {
        let msg = {
            let read = read_message(&mut stream);
            tokio::pin!(read);
            loop {
                if !down {
                    break read.as_mut().await?;
                }
                tokio::select! {
                    msg = read.as_mut() => break msg?,
                    () = tokio::time::sleep(STALL) => {
                        down = false;
                        if let Some((_, d)) = device.as_mut() {
                            d.release_all()?;
                        }
                    }
                }
            }
        };
        if !*heard {
            *heard = true;
            tracing::info!("phone touchpad connected over USB");
        }
        match decode(&msg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))? {
            Msg::Begin { w_mm, h_mm } => {
                if device
                    .as_ref()
                    .is_none_or(|(dims, _)| *dims != (w_mm, h_mm))
                {
                    device = Some(((w_mm, h_mm), open(w_mm, h_mm)?));
                }
            }
            Msg::Frame(points) => {
                down = !points.is_empty();
                if let Some((_, d)) = device.as_mut() {
                    d.apply(&points)?;
                }
            }
            Msg::End => {
                down = false;
                device = None;
            }
        }
    }
}

// ponytail: the tunnel is unauthenticated; another local user who binds PORT
// before iproxy can feed touches. Talk to /run/usbmuxd directly if that matters.
pub async fn drive_touchpad(device_id: String) {
    loop {
        let Some(udid) = udid().await else {
            tokio::time::sleep(NO_DEVICE).await;
            continue;
        };
        ensure_iproxy(&udid).await;
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", PORT)).await {
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

/// First UDID `idevice_id -l` reports, if any device is attached over USB.
async fn udid() -> Option<String> {
    let out = match tokio::process::Command::new("idevice_id")
        .arg("-l")
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            tracing::debug!(error = %e, "could not run idevice_id; is libimobiledevice installed?");
            return None;
        }
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

/// The UDID `iproxy` runs for, and its supervisor: only replaced when the
/// UDID changes, since a dropped connection says nothing about iproxy itself.
static IPROXY: tokio::sync::Mutex<Option<(String, tokio::task::JoinHandle<()>)>> =
    tokio::sync::Mutex::const_new(None);

async fn ensure_iproxy(udid: &str) {
    let mut guard = IPROXY.lock().await;
    if guard.as_ref().is_some_and(|(u, _)| u == udid) {
        return;
    }
    if let Some((_, old)) = guard.take() {
        old.abort();
    }
    *guard = Some((udid.to_string(), tokio::spawn(run_iproxy(udid.to_string()))));
}

/// Keeps `iproxy` running, so a cable reseat or a usbmuxd hiccup heals
/// without a daemon restart. Its output is dropped: it logs every refused connect.
async fn run_iproxy(udid: String) {
    tracing::info!(%udid, port = PORT, "starting iproxy");
    loop {
        match tokio::process::Command::new("iproxy")
            .arg(format!("{PORT}:{PORT}"))
            .arg("-u")
            .arg(&udid)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
        {
            Ok(status) => tracing::info!(%status, "iproxy exited; restarting"),
            Err(e) => {
                tracing::warn!(error = %e, "could not spawn iproxy; is libimobiledevice installed?");
                tokio::time::sleep(RETRY * 4).await;
            }
        }
        tokio::time::sleep(RETRY).await;
    }
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

    fn frame(n: u8) -> Vec<u8> {
        let points = (0..n).map(|id| Point { id, x: 1, y: 2 }).collect();
        message(1, &minicbor::to_vec(Frame { seq: 1, points }).unwrap())
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
            decode(&frame(2)),
            Ok(Msg::Frame(vec![
                TouchPoint { id: 0, x: 1, y: 2 },
                TouchPoint { id: 1, x: 1, y: 2 },
            ]))
        );
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

    type Log = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    struct Fake {
        log: Log,
        dims: (u16, u16),
    }

    impl Fake {
        fn note(&self, line: String) {
            self.log.lock().unwrap().push(line);
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
        TcpStream,
        Log,
        tokio::task::JoinHandle<(std::io::Result<()>, bool)>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let desktop = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut phone, _) = listener.accept().await.unwrap();
        let log = Log::default();
        let opened = log.clone();
        let task = tokio::spawn(async move {
            let mut heard = false;
            let r = session(desktop, "dev", &mut heard, |w, h| {
                opened.lock().unwrap().push(format!("open {w}x{h}"));
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

    async fn say(phone: &mut TcpStream, msg: &[u8]) {
        phone.write_u32(msg.len() as u32).await.unwrap();
        phone.write_all(msg).await.unwrap();
    }

    fn begin(w_mm: u16, h_mm: u16) -> Vec<u8> {
        message(0, &minicbor::to_vec(Begin { w_mm, h_mm }).unwrap())
    }

    async fn finish(
        phone: TcpStream,
        log: Log,
        task: tokio::task::JoinHandle<(std::io::Result<()>, bool)>,
    ) -> (std::io::ErrorKind, bool, Vec<String>) {
        drop(phone);
        let (r, heard) = task.await.unwrap();
        let lines = log.lock().unwrap().clone();
        (r.unwrap_err().kind(), heard, lines)
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

    #[tokio::test]
    async fn a_phone_that_never_speaks_is_not_heard() {
        let (phone, log, task) = start().await;
        let (_, heard, lines) = finish(phone, log, task).await;
        assert!(!heard);
        assert!(lines.is_empty());
    }
}
