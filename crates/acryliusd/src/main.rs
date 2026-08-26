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
use acrylius_core::plugins::{clipboard, command, media, ping, session, share, wol};
use acrylius_linux::effector::LinuxEffector;
use acrylius_rt::effector::Effector;
use acrylius_rt::store::{FileStore, Store};
use acrylius_rt::tcp::TcpTransport;
use acrylius_rt::{Runtime, transport::Transport};
use clap::Parser;
use tokio::sync::{Mutex, broadcast};

const TCP: TransportId = TransportId(1);
/// Higher than TCP, and that is load-bearing rather than arbitrary: the core
/// tries routes in ascending transport order, so Wi-Fi is preferred and BLE is
/// what a peer falls back to.
const BLE: TransportId = TransportId(2);

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

/// Things the installer needs, kept in the binary because that is where the
/// schema is. A shell script that knew which settings exist would be a second
/// copy of the config format to keep in step.
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
    /// For the systemd unit. It runs under `ProtectHome=read-only`, so anything
    /// outside `ReadWritePaths=` fails with "read-only file system" at the
    /// moment it is used rather than at startup — and the download directory is
    /// a config setting, so no unit shipped in this repository can name it. The
    /// installer asks instead.
    WritablePaths,
    /// Write a commented config, if there is none. Never overwrites.
    Init,
    /// Add settings a newer version introduced, leaving everything else alone.
    Update,
    /// Parse it and report anything wrong, without starting.
    Check,
}

/// Expand a leading `~`, which is the only thing a person writing a path in a
/// config file expects to work and the one thing no library call does.
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
/// The public half is always derived from the private half rather than stored
/// beside it, so a file that had been edited cannot produce an identity whose
/// fingerprint lies about the key it can prove possession of.
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

/// What this machine tells a phone about waking it.
///
/// Every field falls back to something the machine can work out about itself,
/// because the alternative is what shipped: a config with `macs = []` and
/// `last_ipv4 = ""`, a phone that shows no wake button, and no indication
/// anywhere that a value was missing rather than the feature being broken.
///
/// `last_ipv4` matters as much as the MAC. A network card matches a magic
/// packet by its payload and ignores the address it was sent to, so a unicast
/// datagram at the machine's last known address wakes it — and that is the only
/// kind iOS can send, since broadcast needs an entitlement a free Apple account
/// cannot have. Announcing an empty one leaves the phone with nowhere to aim.
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

/// A stand-in device id for a plugin verb that broadcasts.
///
/// A local change has no peer attached to it: the plugin decides who hears
/// about it from the peers it has seen connect. The vocabulary requires an
/// identifier here, so this is an obviously-not-real one.
fn broadcast_placeholder() -> acrylius_core::proto::ids::DeviceId {
    acrylius_core::proto::ids::DeviceId::of(&[0u8; 32])
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

/// Config maintenance, and then exit. Nothing here starts a daemon or touches
/// the network.
fn run_config_action(action: &ConfigAction, path: &std::path::Path) -> anyhow::Result<()> {
    let reference = reconcile::reference_text(&config::Config::default())?;

    match action {
        ConfigAction::Path => println!("{}", path.display()),

        ConfigAction::WritablePaths => {
            // From the config as it stands, not from the defaults: somebody who
            // pointed downloads somewhere else needs that path allowed, not the
            // one they did not choose.
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
            // Reported, never removed: a typo and a setting from a version
            // newer than this binary look identical from here.
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
                // `acrylius_linux` is in the default set because a transport lives
                // there now. Without it the BLE transport registered, advertised,
                // took links up and down, and said none of it — which is how a
                // whole transport ran unnoticed for an afternoon.
                .unwrap_or_else(|_| "acryliusd=info,acrylius_rt=info,acrylius_linux=info".into()),
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
    let name = args.name.or_else(|| cfg.name.clone()).unwrap_or_else(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "acrylius".to_string())
    });

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
    // Files are the daemon's business, not the effector's: it is the only
    // place that knows both where a download goes and what a transfer is.
    let bulk = match files::FileBulk::new(
        expand_home(&cfg.share.directory),
        (!cfg.share.advertise_host.is_empty()).then(|| cfg.share.advertise_host.clone()),
    ) {
        Ok(b) => {
            // Checked now rather than during a transfer. Under the systemd
            // unit's `ProtectHome=read-only` a directory outside
            // `ReadWritePaths=` looks entirely normal until something writes to
            // it, and the failure then arrives as "read-only file system" in
            // the middle of receiving a file, pointing at nothing.
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
            ..Default::default()
        },
    )
    .effects(kinds.clone())
    // The same plugin list every device registers. A plugin whose effects this
    // machine cannot serve is dropped by the core and its capability never
    // advertised, so there is nothing to switch on here.
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

    let mut rt = Runtime::new(core, effector, Box::new(store));

    // Keep the control socket's answers live. The closure sees `&Core` and
    // nothing more, so there is deliberately no way to reach `handle()` from
    // here, which is what keeps the single-serial-executor rule intact.
    {
        let devices = devices.clone();
        let status = status.clone();
        rt.observe(move |core| {
            if let Ok(mut d) = devices.try_lock() {
                *d = snapshot_devices(core);
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
    // Registered unconditionally. A machine with no adapter, or one whose
    // controller cannot be a peripheral, answers that in `run` and quietly does
    // nothing — the same "the machine reports what it has" rule the effectors
    // follow, rather than a `#[cfg]` or a config flag that lies on the wrong
    // hardware.
    if cfg.ble.enabled {
        rt.add_transport(
            Arc::new(acrylius_linux::ble::BleTransport::new(BLE, name.clone()))
                as Arc<dyn Transport>,
        );
    }

    // The control socket sees UI events over a broadcast, so several `acryliusctl`
    // invocations can watch at once without stealing each other's events.
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
    // A question for the person at this desktop belongs on their screen. Absent
    // on a machine with no notification daemon, and everything still works
    // through `acryliusctl` — which is why nothing below requires one.
    let prompter = match bulk.clone() {
        Some(bulk) => prompt::Prompter::start(rt.events(), bulk).await,
        None => None,
    };
    let names = devices.clone();
    tokio::spawn(async move {
        while let Some(e) = ui_mpsc_rx.recv().await {
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
                    // An offer has to be remembered before it can be accepted:
                    // by the time a person says yes, the name it chose is all
                    // there is to build a destination from.
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

    let _sock = control::serve(
        control::socket_path(&state, explicit_state),
        control::Handles {
            transport: TCP,
            bulk: bulk.clone(),
            events: rt.events(),
            ui: ui_tx,
            status,
            devices,
        },
    )
    .await?;

    // Watchers. Each one only ever *submits an event*; none of them touches the
    // core, which is what keeps the single-serial-executor rule intact.
    let events = rt.events();
    if kinds.contains(&acrylius_core::vocab::EffectKind::Clipboard) && cfg.clipboard.send {
        let events = events.clone();
        tokio::spawn(async move {
            acrylius_linux::clipboard::watch(move |data| {
                let _ = events.send(acrylius_core::vocab::Event::Local(
                    acrylius_core::vocab::LocalCommand::Plugin {
                        // Broadcast: the plugin sends to every connected peer,
                        // so this identifier is a placeholder it ignores.
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
            // The lock state has two sources and only one of them is on D-Bus:
            // a compositor that does not maintain LockedHint cannot signal at
            // all. So this ticks, and the plugin drops anything unchanged.
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
            // MPRIS does emit PropertiesChanged, but a player is free not to,
            // and several do not until something asks. Without this a phone saw
            // the state from the moment it connected and then nothing until it
            // pressed a button — a remote that only updates when you use it.
            //
            // Two seconds because this is a now-playing display and a track
            // change any later than that reads as broken. The plugin drops a
            // state that has not meaningfully changed, and a position moving on
            // its own does not count, so an idle machine sends nothing.
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

    tokio::select! {
        () = rt.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
