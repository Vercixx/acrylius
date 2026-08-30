//! What this machine offers, and to whom.
//!
//! Everything here is a decision the owner of the machine makes. Nothing in it
//! can be changed from the network: a peer chooses from what this file allows
//! and has no way to add to it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use acrylius_linux::command::{CommandCatalog, CommandSpec};
use acrylius_linux::effector::WolSettings;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// What peers call this machine. Advisory.
    pub name: Option<String>,
    pub port: u16,
    pub wol: WolConfig,
    pub clipboard: ClipboardConfig,
    pub session: SessionConfig,
    pub share: ShareConfig,
    pub ble: BleConfig,
    pub pair: PairConfig,
    /// Commands a paired device may run, keyed by the id that travels.
    pub commands: BTreeMap<String, CommandSpec>,
}

/// Pairing.
///
/// Pairing has no pre-shared key, so any device that can reach this one may
/// start a handshake and put six digits on this screen. Nothing is stored
/// without somebody here pressing a button, and a cooldown keeps a device that
/// keeps asking to one notification — but a machine that is done pairing has no
/// reason to answer at all, and this is how to say so.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PairConfig {
    pub enabled: bool,
}

impl Default for PairConfig {
    fn default() -> Self {
        // On. A machine nobody can pair with is a machine nobody can use, and
        // the first thing anyone does with this software is pair something.
        Self { enabled: true }
    }
}

/// Bluetooth LE.
///
/// Capability is detected, not configured: a machine with no adapter, or one
/// whose controller cannot act as a peripheral, offers no BLE transport whatever
/// this says. What this decides is whether a machine that *can* should — because
/// advertising is a radio broadcasting this machine's presence continuously, and
/// that is the owner's call to make rather than ours to assume.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BleConfig {
    pub enabled: bool,
}

impl Default for BleConfig {
    fn default() -> Self {
        // On, because a transport nobody enables is a transport nobody tests,
        // and the phone reaching the desktop with Wi-Fi off is the point of it.
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WolConfig {
    /// This machine's own interfaces, sent to peers so they can wake it.
    pub macs: Vec<String>,
    pub broadcast: String,
    pub port: u16,
    /// Where a phone should aim first.
    ///
    /// Unicast wakes a machine just as well as broadcast, and iOS cannot
    /// broadcast without an entitlement a free account cannot get. Left empty,
    /// the daemon fills this in with the address it is currently reachable at.
    pub last_ipv4: String,
    /// Other machines this one may be asked to wake. Empty means none.
    pub allowlist: Vec<String>,
}

/// How this machine locks and unlocks, when logind's signal is not enough.
///
/// `loginctl unlock-session` only emits a signal, and acting on it is the
/// screen locker's choice. A lock implemented inside a Wayland shell may offer
/// no way in from outside at all, in which case unlocking remotely is
/// impossible until you say how it is done here. Each is an argv vector, run
/// with no shell.
///
/// Quickshell, for example, needs a handler adding to its own config before
/// there is anything to call:
///
/// ```toml
/// [session]
/// unlock_command = ["qs", "-c", "ii", "ipc", "call", "lock", "deactivate"]
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub lock_command: Vec<String>,
    pub unlock_command: Vec<String>,
}

/// Receiving files.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareConfig {
    /// Where an accepted file is written. Every incoming file lands here and
    /// nowhere else: a peer chooses a name, never a directory.
    pub directory: String,
    /// Accept without asking. Off, because a device that wrote whatever a peer
    /// sent it would be a file drop for anything ever paired with it.
    pub auto_accept: bool,
    /// What to tell a peer to connect to for a transfer. Empty means ask the
    /// kernel which address it would use to reach the network.
    pub advertise_host: String,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self {
            directory: default_download_dir(),
            auto_accept: false,
            advertise_host: String::new(),
        }
    }
}

/// Where this desktop actually puts downloads.
///
/// Not `~/Downloads`. That folder is called `Загрузки` on a Russian system,
/// `Téléchargements` on a French one, and so on; hardcoding the English name
/// invents a second folder beside the real one and quietly fills it with files
/// nobody will think to look in. That is not hypothetical — it is what this did
/// before, and the files went somewhere their owner could not find.
///
/// The answer lives in `user-dirs.dirs`, which `xdg-user-dirs-update` writes and
/// every desktop reads. The environment variable is checked first but is rarely
/// set: it is exported into a session by some setups and not by most.
fn default_download_dir() -> String {
    if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        return dir.to_string_lossy().into_owned();
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    xdg_user_dir("DOWNLOAD", &home)
        .unwrap_or_else(|| home.join("Downloads"))
        .to_string_lossy()
        .into_owned()
}

/// Whether a configured download directory looks like one an earlier version
/// wrote by mistake, and what to do about it.
///
/// A narrow check on purpose. Somebody who deliberately chose a folder is not
/// to be second-guessed, so this fires only on the exact shape the bug left
/// behind: literally `$HOME/Downloads`, on a desktop whose downloads folder is
/// called something else, with nothing in the English one. That combination is
/// not a choice anybody made.
#[must_use]
pub fn stale_download_dir(configured: &str) -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let english = home.join("Downloads");
    if Path::new(configured) != english {
        return None;
    }
    let real = xdg_user_dir("DOWNLOAD", &home)?;
    if real == english || !real.is_dir() {
        return None;
    }
    let empty = std::fs::read_dir(&english)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true);
    empty.then(|| {
        format!(
            "this desktop's downloads folder is {}, and nothing has ever been \
             put in {}. An earlier version guessed the English name. Change \
             share.directory if files should go where the rest of them do.",
            real.display(),
            english.display()
        )
    })
}

/// Read one entry out of `~/.config/user-dirs.dirs`.
///
/// The file is shell syntax, but only barely: a line per directory, always
/// quoted, always either absolute or relative to `$HOME`. Parsing that much is
/// a good deal less trouble than running a shell to do it.
fn xdg_user_dir(name: &str, home: &std::path::Path) -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    xdg_user_dir_in(&config, name, home)
}

/// The parsing, with nowhere to look already decided, so a test needs no
/// environment of its own.
fn xdg_user_dir_in(
    config: &std::path::Path,
    name: &str,
    home: &std::path::Path,
) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
    let key = format!("XDG_{name}_DIR=");
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(value) = line.strip_prefix(&key) else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        // "$HOME/Загрузки" is the usual form. An absolute path is also allowed.
        let path = match value.strip_prefix("$HOME/") {
            Some(rest) => home.join(rest),
            None if value == "$HOME" => home.to_path_buf(),
            None => PathBuf::from(value),
        };
        // A user dir pointing at $HOME itself means "I do not have one of
        // these", and dropping files straight into a home directory is not what
        // anybody wants.
        return (path != home).then_some(path);
    }
    None
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    /// Send local changes to peers.
    pub send: bool,
    /// Apply what peers send.
    pub receive: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: None,
            port: acrylius_proto::DEFAULT_PORT,
            wol: WolConfig::default(),
            clipboard: ClipboardConfig::default(),
            session: SessionConfig::default(),
            share: ShareConfig::default(),
            ble: BleConfig::default(),
            pair: PairConfig::default(),
            commands: BTreeMap::new(),
        }
    }
}

impl Default for WolConfig {
    fn default() -> Self {
        Self {
            macs: Vec::new(),
            broadcast: "255.255.255.255".to_string(),
            port: 9,
            last_ipv4: String::new(),
            allowlist: Vec::new(),
        }
    }
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            send: true,
            receive: true,
        }
    }
}

impl Config {
    /// Read the config, or fall back to defaults when there is none.
    ///
    /// A missing file is normal: a fresh install has nothing to configure and
    /// should still start. A malformed one is not, and refusing to start beats
    /// running with a silently ignored allowlist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "no config; using defaults");
                return Ok(Self::default());
            }
            Err(e) => return Err(e.into()),
        };
        let config: Self = toml::from_str(&text)?;
        config
            .catalog()
            .validate()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(config)
    }

    #[must_use]
    pub fn catalog(&self) -> CommandCatalog {
        CommandCatalog::new(self.commands.clone())
    }

    #[must_use]
    pub fn wol_settings(&self) -> WolSettings {
        WolSettings {
            allowlist: self.wol.allowlist.clone(),
            broadcast: self.wol.broadcast.clone(),
            port: self.wol.port,
        }
    }

    #[must_use]
    pub fn default_path() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            })
            .join("acrylius/config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this exists to prevent: a downloads folder called `Downloads` on
    /// a desktop whose downloads folder is called something else, quietly
    /// collecting files nobody can find.
    #[test]
    fn a_downloads_folder_is_whatever_this_desktop_calls_it() {
        let dir = std::env::temp_dir().join(format!("acr-xdg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config")).unwrap();
        std::fs::write(
            dir.join(".config/user-dirs.dirs"),
            "# This file is written by xdg-user-dirs-update\n\
             XDG_DESKTOP_DIR=\"$HOME/Рабочий стол\"\n\
             XDG_DOWNLOAD_DIR=\"$HOME/Загрузки\"\n",
        )
        .unwrap();
        let config = dir.join(".config");
        assert_eq!(
            xdg_user_dir_in(&config, "DOWNLOAD", &dir),
            Some(dir.join("Загрузки")),
            "the name this desktop actually uses"
        );
        assert_eq!(
            xdg_user_dir_in(&config, "VIDEOS", &dir),
            None,
            "and nothing invented for one it does not have"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_are_usable_without_a_file() {
        let c = Config::default();
        assert!(
            c.commands.is_empty(),
            "nothing is runnable until someone says so"
        );
        assert!(
            c.wol.allowlist.is_empty(),
            "nothing is wakeable until someone says so"
        );
        assert!(c.clipboard.send && c.clipboard.receive);
    }

    #[test]
    fn a_command_with_a_relative_program_stops_startup() {
        // Better to refuse to start than to run with a command that will fail
        // in a surprising way the first time somebody taps it.
        let toml = r#"
            [commands.oops]
            name = "Oops"
            program = "grim"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.catalog().validate().is_err());
    }

    #[test]
    fn a_full_config_round_trips() {
        let toml = r#"
            name = "desktop"
            port = 1971

            [wol]
            macs = ["00:11:22:33:44:55"]
            broadcast = "192.168.1.255"
            port = 9
            allowlist = ["aa:bb:cc:dd:ee:ff"]

            [clipboard]
            send = true
            receive = false

            [commands.screenshot]
            name = "Take a screenshot"
            program = "/usr/bin/grim"
            args = ["-g", "0,0 100x100"]
            needs_confirm = false
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.name.as_deref(), Some("desktop"));
        assert!(!c.clipboard.receive);
        assert_eq!(c.wol.allowlist.len(), 1);
        assert_eq!(c.catalog().manifest().len(), 1);
        assert!(c.catalog().validate().is_ok());
    }
}
