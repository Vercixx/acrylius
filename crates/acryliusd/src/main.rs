//! The acrylius daemon.
//!
//! Runs as your user, not root. That is not a compromise: logind passes the
//! session owner's uid to polkit as `good_user`, which short-circuits the check
//! when the caller's uid matches, so locking and unlocking your own session
//! needs no sudo, no setuid binary and no polkit rule. Nothing here requires
//! privilege, which is what lets the systemd unit be locked down hard.

mod config;
mod control;
mod files;
mod netself;
mod prompt;
mod reconcile;

use std::path::PathBuf;
use std::sync::Arc;

use acrylius_core::config::CoreConfig;
use acrylius_core::core::CoreBuilder;
use acrylius_core::link::TransportId;
use acrylius_core::noise::Identity;
use acrylius_core::peer::PeerState;
use acrylius_core::plugins::{clipboard, command, media, ping, session, share, touchpad, wol};
use acrylius_linux::effector::LinuxEffector;
use acrylius_rt::effector::Effector;
use acrylius_rt::store::{FileStore, Store};
use acrylius_rt::tcp::TcpTransport;
use acrylius_rt::{Runtime, transport::Transport};
use clap::Parser;
use tokio::sync::{Mutex, broadcast};

const TCP: TransportId = TransportId(1);
/// Higher than TCP on purpose: the core tries routes in ascending order, so
/// Wi-Fi is preferred and BLE is the fallback.
const BLE: TransportId = TransportId(2);
/// Lower than TCP: `connect_peer` only auto-upgrades a `Reachable` peer to a
/// *strictly lower* transport id, so a mid-session cable plug needs this.
const USB: TransportId = TransportId(0);

#[derive(Parser, Debug)]
#[command(name = "acryliusd", version, about = "The acrylius daemon")]
struct Args {
    /// TCP port to listen on. Override to run a second instance on one machine.
    #[arg(long, default_value_t = acrylius_proto::DEFAULT_PORT)]
    port: u16,
    /// Where identity, peers and the control socket live.
    #[arg(long, env = "ACRYLIUS_STATE")]
    state: Option<PathBuf>,
    /// The name peers see. Advisory only; never used for a policy decision.
    #[arg(long)]
    name: Option<String>,
    /// Where the config lives.
    #[arg(long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Config subcommands, kept in the binary since that's where the schema lives.
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Config file maintenance.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(clap::Subcommand, Debug)]
enum ConfigAction {
    /// Print where the config file lives.
    Path,
    /// Print every directory the daemon must be able to write to, one per line.
    ///
    /// For the systemd unit's `ReadWritePaths=`: the download directory is a
    /// runtime setting, so no shipped unit can name it in advance.
    WritablePaths,
    /// Write a commented config, if there is none. Never overwrites.
    Init,
    /// Add settings a newer version introduced, leaving everything else alone.
    Update,
    /// Parse it and report anything wrong, without starting.
    Check,
}

/// Expand a leading `~` in a path; no library call does this.
fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(rest.trim_start_matches('/'))
}

fn state_dir(arg: Option<PathBuf>) -> PathBuf {
    arg.unwrap_or_else(|| {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
            })
            .join("acrylius")
    })
}

/// Load the static identity, or make one on first run.
///
/// The public half is derived from the private half, never stored, so an
/// edited file can't produce a mismatched fingerprint.
fn load_identity(state: &std::path::Path) -> anyhow::Result<Identity> {
    let path = state.join("identity.key");
    if let Ok(bytes) = std::fs::read(&path) {
        let key: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} is not a 32-byte key", path.display()))?;
        return Ok(Identity::from_private(key));
    }
    let id = Identity::generate()?;
    std::fs::write(&path, id.private())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(fingerprint = %id.fingerprint(), "generated a new identity");
    Ok(id)
}

/// What this machine tells a phone about waking it; every field falls back to
/// a machine-derived value, since a blank one just hides the wake button.
///
/// `last_ipv4` matters as much as the MAC: a NIC wakes on packet payload, not
/// destination, and it's the only address iOS can unicast to (broadcast needs
/// an entitlement free accounts lack).
fn wake_config(cfg: &config::WolConfig) -> wol::WolConfig {
    let here = netself::routed_ipv4();
    let macs = if cfg.macs.is_empty() {
        netself::wakeable_macs()
    } else {
        cfg.macs.clone()
    };
    let last_ipv4 = if cfg.last_ipv4.is_empty() {
        here.clone().unwrap_or_default()
    } else {
        cfg.last_ipv4.clone()
    };
    let broadcast = if cfg.broadcast.is_empty() {
        here.as_deref()
            .map(netself::broadcast_for)
            .unwrap_or_default()
    } else {
        cfg.broadcast.clone()
    };
    if macs.is_empty() {
        tracing::warn!(
            "no wakeable network card found, and none configured: this machine \
             cannot be woken remotely. Set wol.macs if it has one."
        );
    } else {
        tracing::info!(?macs, %last_ipv4, "this machine can be woken at");
    }
    wol::WolConfig {
        macs,
        broadcast,
        port: cfg.port,
        last_ipv4,
    }
}

/// A stand-in device id for a plugin verb that broadcasts to every peer.
///
/// The vocabulary requires an identifier here even though a local change has
/// no single peer attached; this is an obviously-not-real one.
fn broadcast_placeholder() -> acrylius_core::proto::ids::DeviceId {
    acrylius_core::proto::ids::DeviceId::of(&[0u8; 32])
}

/// First UDID `idevice_id -l` reports, if any device is attached over USB.
async fn usb_udid() -> Option<String> {
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
    if !out.status.success() {
        tracing::debug!(
            status = %out.status,
            stderr = %String::from_utf8_lossy(&out.stderr),
            "idevice_id -l did not succeed"
        );
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

fn snapshot_devices(core: &acrylius_core::core::Core) -> Vec<control::Device> {
    core.peers()
        .filter_map(|p| {
            let id = p.id()?;
            Some(control::Device {
                device_id: id.to_string(),
                name: p.name.clone(),
                platform: p.platform.clone(),
                fingerprint: p.fingerprint()?.to_string(),
                reachable: core.peer_state(&id) == PeerState::Reachable,
            })
        })
        .collect()
}

/// What to call this machine when nobody has said.
///
/// Prefers the pretty hostname (may have spaces or non-Latin characters) over
/// the DNS-label-only static one. Read from `/etc/machine-info` directly
/// rather than over D-Bus, since the daemon starts before assuming a system bus exists.
fn machine_name() -> String {
    if let Ok(info) = std::fs::read_to_string("/etc/machine-info") {
        for line in info.lines() {
            let Some(value) = line.trim().strip_prefix("PRETTY_HOSTNAME=") else {
                continue;
            };
            // hostnamectl quotes the value or not, depending on content.
            let value = value.trim().trim_matches(['"', '\'']).trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "acrylius".to_string())
}

/// What is on this network that this machine is not paired with; the phone
/// gets the same facts via `UiEvent::Discovered`.
fn snapshot_nearby(core: &acrylius_core::core::Core) -> Vec<control::Nearby> {
    core.nearby()
        .map(|n| control::Nearby {
            fingerprint: n.fingerprint.to_string(),
            name: n.name.to_string(),
            addr: n.addr,
            transport: n.transport.0,
            pairing: n.pairing,
        })
        .collect()
}

/// Config maintenance, and then exit. Nothing here starts a daemon or touches
/// the network.
fn run_config_action(action: &ConfigAction, path: &std::path::Path) -> anyhow::Result<()> {
    let reference = reconcile::reference_text(&config::Config::default())?;

    match action {
        ConfigAction::Path => println!("{}", path.display()),

        ConfigAction::WritablePaths => {
            // From the config as-is, not defaults: the path actually in use needs to be allowed.
            let cfg = config::Config::load(path).unwrap_or_default();
            let dir = expand_home(&cfg.share.directory);
            if !dir.as_os_str().is_empty() {
                println!("{}", dir.display());
            }
        }

        ConfigAction::Init => {
            if path.exists() {
                println!("{} exists; left alone", path.display());
                return Ok(());
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, reconcile::commented_default(&reference))?;
            println!("wrote {}", path.display());
        }

        ConfigAction::Update => {
            if !path.exists() {
                println!("no config at {}; nothing to update", path.display());
                return Ok(());
            }
            let added = reconcile::update_file(path, &reference)?;
            if added.is_empty() {
                println!("{} is up to date", path.display());
            } else {
                println!("added to {}:", path.display());
                for (table, keys) in reconcile::describe(&added) {
                    let where_ = if table.is_empty() {
                        "(top level)".to_string()
                    } else {
                        format!("[{table}]")
                    };
                    println!("  {where_}  {}", keys.join(", "));
                }
            }
            // Reported, never removed: a typo and a newer-version setting look identical here.
            let text = std::fs::read_to_string(path)?;
            let unknown = reconcile::unknown_keys(&text, &reference);
            if !unknown.is_empty() {
                println!("settings this version does not know about, left in place:");
                for key in unknown {
                    println!("  {key}");
                }
            }
        }

        ConfigAction::Check => {
            let cfg = config::Config::load(path)?;
            println!("{} parses", path.display());
            println!("  port       {}", cfg.port);
            println!("  commands   {}", cfg.commands.len());
            println!(
                "  clipboard  send {}, receive {}",
                cfg.clipboard.send, cfg.clipboard.receive
            );
            let session_override =
                !cfg.session.lock_command.is_empty() || !cfg.session.unlock_command.is_empty();
            println!(
                "  session    {}",
                if session_override {
                    "using configured commands"
                } else {
                    "using logind"
                }
            );
            println!("  files      land in {}", cfg.share.directory);
            if let Some(note) = config::stale_download_dir(&cfg.share.directory) {
                println!("  NOTE       {note}");
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // acrylius_linux and acrylius_core are in the default filter too:
                // both can run silently otherwise, hiding real transport/routing issues.
                .unwrap_or_else(|_| {
                    "acryliusd=info,acrylius_rt=info,acrylius_linux=info,acrylius_core=info".into()
                }),
        )
        .init();

    let args = Args::parse();
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(config::Config::default_path);

    if let Some(Command::Config { action }) = &args.command {
        return run_config_action(action, &config_path);
    }

    let explicit_state = args.state.is_some();
    let state = state_dir(args.state);
    std::fs::create_dir_all(&state)?;

    let cfg = config::Config::load(&config_path)?;
    let port = if args.port == acrylius_proto::DEFAULT_PORT {
        cfg.port
    } else {
        args.port
    };

    let identity = load_identity(&state)?;
    let store = FileStore::open(&state)?;
    let peers = store.load_peers()?;
    let name = args
        .name
        .or_else(|| cfg.name.clone())
        .unwrap_or_else(machine_name);

    tracing::info!(
        %name,
        device_id = %identity.device_id(),
        fingerprint = %identity.fingerprint(),
        peers = peers.len(),
        "starting"
    );

    let effector = Arc::new(
        LinuxEffector::new(
            cfg.catalog(),
            cfg.wol_settings(),
            acrylius_linux::session::Commands {
                lock: cfg.session.lock_command.clone(),
                unlock: cfg.session.unlock_command.clone(),
            },
        )
        .await,
    );
    // Files are the daemon's business: it's the only place that knows both
    // the download dir and the transfer.
    let bulk = match files::FileBulk::new(
        expand_home(&cfg.share.directory),
        (!cfg.share.advertise_host.is_empty()).then(|| cfg.share.advertise_host.clone()),
    ) {
        Ok(b) => {
            // Checked now, not during a transfer: under ProtectHome=read-only,
            // a bad dir looks fine until something actually writes to it.
            match b.writable() {
                Ok(()) => tracing::info!(dir = %b.dir().display(), "files sent here land in"),
                Err(e) => tracing::warn!(
                    dir = %b.dir().display(), error = %e,
                    "cannot write to the download directory, so no file can be received. \
                     If this daemon runs under systemd, its unit has to allow that path: \
                     re-run scripts/install.sh, which adds one for whatever share.directory says."
                ),
            }
            Some(Arc::new(b))
        }
        Err(e) => {
            tracing::warn!(dir = %cfg.share.directory, error = %e, "no download directory; receiving files is off");
            None
        }
    };

    let mut kinds = effector.supported();
    if bulk.is_some() {
        kinds.push(acrylius_core::vocab::EffectKind::Share);
    }
    tracing::info!(?kinds, "this machine can");

    let core = CoreBuilder::new(
        identity,
        CoreConfig {
            name: name.clone(),
            platform: "linux".to_string(),
            accept_pair_requests: cfg.pair.enabled,
            ..Default::default()
        },
    )
    .effects(kinds.clone())
    // Same plugin list on every device; the core drops one whose effects this
    // machine can't serve.
    .plugin(ping::PingPlugin::default())
    .plugin(session::SessionPlugin::default())
    .plugin(wol::WolPlugin::new(
        wake_config(&cfg.wol),
        cfg.wol.allowlist.clone(),
    ))
    .plugin(clipboard::ClipboardPlugin::new(clipboard::Directions {
        send: cfg.clipboard.send,
        receive: cfg.clipboard.receive,
    }))
    .plugin(command::CommandPlugin::new(effector.catalog().manifest()))
    .plugin(media::MediaPlugin::default())
    .plugin(share::SharePlugin::default())
    .plugin(touchpad::TouchpadPlugin::default())
    .restore(peers)
    .build();

    let status = control::Status {
        name: name.clone(),
        device_id: core.device_id().to_string(),
        fingerprint: core.fingerprint().to_string(),
        port,
        peers: core.peers().count(),
        caps_in: core.caps_in().to_vec(),
        caps_out: core.caps_out().to_vec(),
    };
    let fingerprint = core.fingerprint();
    let status = Arc::new(Mutex::new(Some(status)));
    let devices = Arc::new(Mutex::new(snapshot_devices(&core)));
    let nearby = Arc::new(Mutex::new(snapshot_nearby(&core)));
    let pending_pair: Arc<Mutex<Option<control::Confirmation>>> = Arc::new(Mutex::new(None));

    let mut rt = Runtime::new(core, effector, Box::new(store));

    // Closure only sees &Core, no way to reach handle() — keeps the
    // single-serial-executor rule intact.
    {
        let devices = devices.clone();
        let nearby = nearby.clone();
        let pending_pair = pending_pair.clone();
        let status = status.clone();
        rt.observe(move |core| {
            if let Ok(mut d) = devices.try_lock() {
                *d = snapshot_devices(core);
            }
            if let Ok(mut n) = nearby.try_lock() {
                *n = snapshot_nearby(core);
            }
            if let Ok(mut p) = pending_pair.try_lock() {
                *p = core.pending_pairing().map(|q| control::Confirmation {
                    name: q.name.to_string(),
                    fingerprint: q.fingerprint.to_string(),
                    sas: q.sas.to_string(),
                });
            }
            if let Ok(mut s) = status.try_lock()
                && let Some(s) = s.as_mut()
            {
                s.peers = core.peers().count();
            }
        });
    }
    rt.add_transport(
        Arc::new(TcpTransport::new(TCP, port, fingerprint, name.clone())) as Arc<dyn Transport>,
    );
    // Registered unconditionally; a machine with no adapter or peripheral
    // support just no-ops in run().
    if cfg.ble.enabled {
        rt.add_transport(
            Arc::new(acrylius_linux::ble::BleTransport::new(BLE, name.clone()))
                as Arc<dyn Transport>,
        );
    }
    if cfg.usb.enabled {
        rt.add_transport(
            Arc::new(acrylius_linux::usb::UsbTransport::new(USB)) as Arc<dyn Transport>
        );
    }

    // UI events go out over a broadcast channel so multiple acryliusctl
    // invocations can watch at once.
    let (ui_tx, _) = broadcast::channel(256);
    let (ui_mpsc_tx, mut ui_mpsc_rx) = tokio::sync::mpsc::unbounded_channel();
    if let Some(bulk) = bulk.clone() {
        rt.set_bulk(bulk);
    }
    rt.set_ui(ui_mpsc_tx);
    let fanout = ui_tx.clone();
    let offers_bulk = bulk.clone();
    let auto_accept = cfg.share.auto_accept;
    let auto_events = rt.events();
    // Not conditional on file sharing: pairing is unrelated, and gating this on
    // `bulk` used to silently break pairing prompts when share was disabled.
    //
    // Started off the startup path: GetCapabilities can be D-Bus activated and
    // take seconds, which would otherwise stall the control socket at boot.
    let prompter: Arc<tokio::sync::OnceCell<Option<Arc<prompt::Prompter>>>> =
        Arc::new(tokio::sync::OnceCell::new());
    {
        let cell = prompter.clone();
        let events = rt.events();
        let bulk = bulk.clone();
        tokio::spawn(async move {
            let _ = cell.set(prompt::Prompter::start(events, bulk).await);
        });
    }
    let names = devices.clone();
    tokio::spawn(async move {
        while let Some(e) = ui_mpsc_rx.recv().await {
            // None until the connection lands; a question in that window goes
            // unprompted, acryliusctl still answers it either way.
            let prompter = prompter.get().and_then(Option::as_ref);
            // The daemon has no screen; this is the only place a core error gets logged.
            if let acrylius_core::vocab::UiEvent::Error { peer, code, detail } = &e {
                tracing::warn!(
                    peer = peer.as_ref().map(ToString::to_string),
                    code = code.as_str(),
                    detail,
                    "core reported an error"
                );
            }
            if let Some(prompter) = &prompter {
                match &e {
                    acrylius_core::vocab::UiEvent::PairingSas {
                        name,
                        fingerprint,
                        sas,
                    } => {
                        // Log the digits themselves: on a machine with no
                        // notification daemon, journalctl is the only surface.
                        tracing::info!(
                            %name, %fingerprint, %sas,
                            "a device asked to pair; run `acryliusctl pair` to answer"
                        );
                        prompter.ask_pair(name, &fingerprint.to_string(), sas).await;
                    }
                    // Close on any resolution (CLI, lapse, etc.) so an answered
                    // question can't be answered twice.
                    acrylius_core::vocab::UiEvent::PairingComplete { .. }
                    | acrylius_core::vocab::UiEvent::PairingFailed { .. } => {
                        prompter.close_pair().await;
                    }
                    _ => {}
                }
            }
            if let acrylius_core::vocab::UiEvent::Plugin {
                peer,
                cap,
                ty,
                body,
            } = &e
                && cap == share::CAP
                && let Some(bulk) = &offers_bulk
            {
                match ty.as_str() {
                    // Remember the offer before it can be accepted: the name is
                    // all we have to build a destination from.
                    "offer" => {
                        if let Ok(offer) = minicbor::decode::<share::Offer>(body) {
                            bulk.note_offer(&peer.to_string(), offer.clone());
                            tracing::info!(
                                name = %offer.name, size = offer.size, transfer = offer.transfer,
                                "a file was offered"
                            );
                            if auto_accept {
                                let body = minicbor::to_vec(share::Finished {
                                    transfer: offer.transfer,
                                    ok: true,
                                    detail: String::new(),
                                })
                                .unwrap_or_default();
                                let _ = auto_events.send(acrylius_core::vocab::Event::Local(
                                    acrylius_core::vocab::LocalCommand::Plugin {
                                        peer: peer.clone(),
                                        cap: share::CAP.to_string(),
                                        ty: "accept".to_string(),
                                        body,
                                    },
                                ));
                            } else if let Some(prompter) = &prompter {
                                let from = names
                                    .lock()
                                    .await
                                    .iter()
                                    .find(|d| d.device_id == peer.to_string())
                                    .map_or_else(|| "A device".to_string(), |d| d.name.clone());
                                prompter.ask(&peer.to_string(), &from, &offer).await;
                            }
                        }
                    }
                    "finished" => {
                        if let Ok(f) = minicbor::decode::<share::Finished>(body)
                            && let Some(prompter) = &prompter
                        {
                            prompter.done(bulk, f.transfer, f.ok, &f.detail).await;
                        }
                    }
                    _ => {}
                }
            }
            let _ = fanout.send(e);
        }
    });

    let devices_for_usb = devices.clone();

    let _sock = control::serve(
        control::socket_path(&state, explicit_state),
        control::Handles {
            transport: TCP,
            bulk: bulk.clone(),
            events: rt.events(),
            ui: ui_tx,
            status,
            devices,
            nearby,
            pending_pair,
        },
    )
    .await?;

    // Watchers only ever submit an event; none touch the core directly —
    // keeps the single-serial-executor rule intact.
    let events = rt.events();
    if kinds.contains(&acrylius_core::vocab::EffectKind::Clipboard) && cfg.clipboard.send {
        let events = events.clone();
        tokio::spawn(async move {
            acrylius_linux::clipboard::watch(move |data| {
                let _ = events.send(acrylius_core::vocab::Event::Local(
                    acrylius_core::vocab::LocalCommand::Plugin {
                        // Placeholder id; the plugin broadcasts to every peer and ignores it.
                        peer: broadcast_placeholder(),
                        cap: clipboard::CAP.to_string(),
                        ty: "changed".to_string(),
                        body: data,
                    },
                ));
            })
            .await;
        });
    }
    if kinds.contains(&acrylius_core::vocab::EffectKind::Session) {
        let events = events.clone();
        tokio::spawn(async move {
            // LockedHint isn't maintained by every compositor, so this polls
            // instead of relying solely on D-Bus signals.
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                interval.tick().await;
                if events
                    .send(acrylius_core::vocab::Event::Local(
                        acrylius_core::vocab::LocalCommand::Plugin {
                            peer: broadcast_placeholder(),
                            cap: session::CAP.to_string(),
                            ty: "notify".to_string(),
                            body: Vec::new(),
                        },
                    ))
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    if kinds.contains(&acrylius_core::vocab::EffectKind::Media) {
        let events = events.clone();
        tokio::spawn(async move {
            // MPRIS players aren't required to emit PropertiesChanged, so this
            // polls instead; the plugin drops unchanged state, so an idle
            // machine sends nothing.
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                if events
                    .send(acrylius_core::vocab::Event::Local(
                        acrylius_core::vocab::LocalCommand::Plugin {
                            peer: broadcast_placeholder(),
                            cap: media::CAP.to_string(),
                            ty: "notify".to_string(),
                            body: Vec::new(),
                        },
                    ))
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    if cfg.usb.enabled {
        let events = events.clone();
        let devices = devices_for_usb;
        tokio::spawn(async move {
            let mut last: Option<String> = None;
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let seen = usb_udid().await;
                let peers: Vec<acrylius_core::proto::ids::DeviceId> = devices
                    .lock()
                    .await
                    .iter()
                    .filter_map(|d| acrylius_core::proto::ids::DeviceId::parse(&d.device_id).ok())
                    .collect();
                if seen != last {
                    tracing::info!(udid = ?seen, was = ?last, peers = peers.len(), "USB attach state changed");
                }
                // Re-sent every tick while attached, not just on the edge, so
                // a peer paired after the cable went in still gets offered USB.
                if let Some(udid) = &seen {
                    for peer in &peers {
                        let _ = events.send(acrylius_core::vocab::Event::Local(
                            acrylius_core::vocab::LocalCommand::SetPeerAddress {
                                peer: peer.clone(),
                                transport: USB,
                                addr: udid.clone(),
                            },
                        ));
                    }
                    let _ = events.send(acrylius_core::vocab::Event::Local(
                        acrylius_core::vocab::LocalCommand::ReconsiderRoutes,
                    ));
                } else if let Some(udid) = &last {
                    for peer in &peers {
                        let _ = events.send(acrylius_core::vocab::Event::Local(
                            acrylius_core::vocab::LocalCommand::ForgetPeerAddress {
                                peer: peer.clone(),
                                transport: USB,
                                addr: udid.clone(),
                            },
                        ));
                    }
                }
                last = seen;
            }
        });
    }

    // systemd stops services with SIGTERM, never SIGINT, so both need to
    // trigger a clean shutdown.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        () = rt.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
        _ = term.recv() => tracing::info!("shutting down"),
    }
    Ok(())
}
