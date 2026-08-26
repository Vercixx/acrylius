//! The acrylius daemon.
//!
//! Runs as your user, not root. That is not a compromise: logind passes the
//! session owner's uid to polkit as `good_user`, which short-circuits the check
//! when the caller's uid matches, so locking and unlocking your own session
//! needs no sudo, no setuid binary and no polkit rule. Nothing here requires
//! privilege, which is what lets the systemd unit be locked down hard.

mod config;
mod control;

use std::path::PathBuf;
use std::sync::Arc;

use acrylius_core::config::CoreConfig;
use acrylius_core::core::CoreBuilder;
use acrylius_core::link::TransportId;
use acrylius_core::noise::Identity;
use acrylius_core::peer::PeerState;
use acrylius_core::plugins::{clipboard, command, ping, session, wol};
use acrylius_linux::effector::LinuxEffector;
use acrylius_rt::effector::Effector;
use acrylius_rt::store::{FileStore, Store};
use acrylius_rt::tcp::TcpTransport;
use acrylius_rt::{Runtime, transport::Transport};
use clap::Parser;
use tokio::sync::{Mutex, broadcast};

const TCP: TransportId = TransportId(1);

#[derive(Parser, Debug)]
#[command(name = "acryliusd", about = "The acrylius daemon")]
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "acryliusd=info,acrylius_rt=info".into()),
        )
        .init();

    let args = Args::parse();
    let explicit_state = args.state.is_some();
    let state = state_dir(args.state);
    std::fs::create_dir_all(&state)?;

    let config_path = args.config.unwrap_or_else(config::Config::default_path);
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
    let kinds = effector.supported();
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
        wol::WolConfig {
            macs: cfg.wol.macs.clone(),
            broadcast: cfg.wol.broadcast.clone(),
            port: cfg.wol.port,
            last_ipv4: cfg.wol.last_ipv4.clone(),
        },
        cfg.wol.allowlist.clone(),
    ))
    .plugin(clipboard::ClipboardPlugin::new(clipboard::Directions {
        send: cfg.clipboard.send,
        receive: cfg.clipboard.receive,
    }))
    .plugin(command::CommandPlugin::new(effector.catalog().manifest()))
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

    // The control socket sees UI events over a broadcast, so several `acryliusctl`
    // invocations can watch at once without stealing each other's events.
    let (ui_tx, _) = broadcast::channel(256);
    let (ui_mpsc_tx, mut ui_mpsc_rx) = tokio::sync::mpsc::unbounded_channel();
    rt.set_ui(ui_mpsc_tx);
    let fanout = ui_tx.clone();
    tokio::spawn(async move {
        while let Some(e) = ui_mpsc_rx.recv().await {
            let _ = fanout.send(e);
        }
    });

    let _sock = control::serve(
        control::socket_path(&state, explicit_state),
        control::Handles {
            transport: TCP,
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

    tokio::select! {
        () = rt.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
