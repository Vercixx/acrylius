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
    /// Commands a paired device may run, keyed by the id that travels.
    pub commands: BTreeMap<String, CommandSpec>,
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
