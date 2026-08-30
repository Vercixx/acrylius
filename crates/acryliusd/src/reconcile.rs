//! Bringing an existing config file up to date without trampling it.
//!
//! Adding a setting to the daemon should not mean a user finds out by reading
//! release notes. `serde(default)` means an old file keeps working, so nothing
//! breaks — but a setting nobody can see is a setting nobody uses, which is how
//! `session.unlock_command` would have gone unnoticed by everyone who needed it.
//!
//! So an update fills in what is missing and touches nothing else. That rules
//! out the obvious implementation: parse to a `Config`, serialise it back, done.
//! It would work, and it would silently delete every comment the user wrote and
//! reorder everything they arranged. `toml_edit` keeps the document as written
//! and edits it in place, which is the whole reason it is a dependency.
//!
//! Two rules, and both matter:
//!
//! * A key that is present is never touched, whatever its value. Someone who
//!   set `send = false` meant it, and an "update" that reset it to the default
//!   would be a bug with a very long feedback loop.
//! * A table the user owns is never filled in. `[commands]` is theirs; a
//!   default has nothing to say about it.

use std::collections::BTreeMap;
use std::path::Path;

use toml_edit::{DocumentMut, Table};

/// Tables whose contents belong to the user, not to the schema.
///
/// Their absence is filled in — an empty `[commands]` is a useful thing to see —
/// but what is inside them is never added to.
const USER_OWNED: &[&str] = &["commands"];

/// What an update changed, so it can be reported rather than done silently.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Added {
    /// Dotted paths, in the order they were inserted.
    pub keys: Vec<String>,
}

impl Added {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Add every setting the schema has and the document lacks.
///
/// Returns the text to write and what was added. The input is returned
/// unchanged when there is nothing to add, so a caller can skip the write.
pub fn reconcile(existing: &str, reference: &str) -> anyhow::Result<(String, Added)> {
    let mut doc: DocumentMut = existing.parse()?;
    let reference: DocumentMut = reference.parse()?;
    let mut added = Added::default();

    merge_table(doc.as_table_mut(), reference.as_table(), "", &mut added);

    if added.is_empty() {
        return Ok((existing.to_string(), added));
    }
    Ok((doc.to_string(), added))
}

fn merge_table(into: &mut Table, from: &Table, prefix: &str, added: &mut Added) {
    for (key, value) in from.iter() {
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };

        match into.get_mut(key) {
            // Present already. Leave it exactly as the user wrote it, value and
            // comments alike, and recurse only to reach settings nested deeper.
            Some(existing) => {
                if USER_OWNED.contains(&path.as_str()) {
                    continue;
                }
                if let (Some(into_t), Some(from_t)) = (existing.as_table_mut(), value.as_table()) {
                    merge_table(into_t, from_t, &path, added);
                }
            }
            None => {
                into.insert(key, value.clone());
                added.keys.push(path);
            }
        }
    }
}

/// The schema as TOML, from the defaults, for `reconcile` to draw on.
pub fn reference_text(config: &crate::config::Config) -> anyhow::Result<String> {
    Ok(toml::to_string_pretty(config)?)
}

/// Write a config for a machine that has none.
///
/// Commented, because a file full of bare defaults teaches nobody what they are
/// for, and this is the only moment anyone reads it.
#[must_use]
pub fn commented_default(reference: &str) -> String {
    let mut out = String::new();
    out.push_str(
        "# acrylius\n\
         #\n\
         # Every value here is a decision this machine makes. Nothing in it can be\n\
         # changed from the network: a paired device chooses from what this file\n\
         # allows and has no way to add to it.\n\
         #\n\
         # `acryliusd config update` adds settings introduced by a newer version\n\
         # and leaves everything you have written alone.\n\n",
    );
    out.push_str(reference);
    out.push_str(&format!("\n{}", HINTS.trim_start_matches('\n')));
    out
}

const HINTS: &str = r#"
# --- what the settings mean -------------------------------------------------
#
# [wol]
#   macs        this machine's own interfaces, sent to paired devices so they
#               can wake it. A phone aims at last_ipv4 first, because iOS gates
#               broadcast behind an entitlement a free developer account cannot
#               get, and a network card matches the packet's payload rather than
#               its destination address.
#
#               Left empty, every network card with real hardware behind it is
#               found and announced, along with this machine's routed address.
#               Set them only to override that. Filling one in by hand and
#               getting it wrong looks identical to the feature not existing.
#   allowlist   MACs this machine will relay a wake to on request. Empty means
#               it relays for nobody, which is the right default.
#
# [clipboard]
#   send        forward what happens here to paired devices.
#   receive     apply what they send.
#
# [session]
#   lock_command, unlock_command
#               argv vectors, run with no shell. Leave empty to use logind.
#
#               logind only emits a signal, and acting on it is the screen
#               locker's choice — plenty do not. A lock implemented inside a
#               Wayland shell may offer no way in from outside at all, in which
#               case remote unlocking is impossible until you say how it is
#               done. Setting this hands out your session without a password, so
#               it is off unless you turn it on.
#
# [ble]
#   enabled     advertise over Bluetooth LE, so a paired phone reaches this
#               machine with Wi-Fi off. A machine with no adapter, or one whose
#               controller cannot act as a peripheral, offers nothing here
#               whatever this says — capability is detected, not configured.
#
#               What it does turn off is the advertising itself, which is a radio
#               announcing this machine continuously. Pairing is unaffected: it
#               happens over the network and never over Bluetooth.
#
# [pair]
#   enabled     answer devices asking to pair. On.
#
#               Pairing has no code and no shared secret, so any device that can
#               reach this one may start a handshake and put six digits on this
#               screen. That is the point: you pair by tapping a computer in the
#               phone's list, not by reading a code off this one.
#
#               What protects it is that those six digits are also on the
#               asking device, and nothing is stored until somebody here presses
#               "They match". A device that keeps asking gets one notification
#               and then a cooldown. Turn this off on a machine that is done
#               pairing, or on a network you would rather nobody could raise a
#               dialog from.
#
# [commands]
#   Commands a paired device may run, keyed by the id that travels. The id is
#   what goes over the network, never the command line, so a device cannot ask
#   for anything not listed here.
#
#   [commands.screenshot]
#   name = "Screenshot"
#   program = "/usr/bin/grim"      # absolute; a relative path stops startup
#   args = ["/home/you/shot.png"]
#   needs_confirm = false
#   timeout_secs = 10
"#;

/// Load, reconcile and write back. Returns what was added.
pub fn update_file(path: &Path, reference: &str) -> anyhow::Result<Added> {
    let existing = std::fs::read_to_string(path)?;
    let (text, added) = reconcile(&existing, reference)?;
    if added.is_empty() {
        return Ok(added);
    }
    // Write beside and rename, so an interrupted update leaves the old config
    // rather than half of one.
    let tmp = path.with_extension("toml.new");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(added)
}

/// Settings the file has that the schema does not.
///
/// Not removed — a typo and a setting from a newer version look identical from
/// here, and deleting the second would be worse than mentioning the first.
#[must_use]
pub fn unknown_keys(existing: &str, reference: &str) -> Vec<String> {
    let (Ok(doc), Ok(reference)) = (
        existing.parse::<DocumentMut>(),
        reference.parse::<DocumentMut>(),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_unknown(doc.as_table(), reference.as_table(), "", &mut out);
    out
}

fn collect_unknown(doc: &Table, reference: &Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in doc.iter() {
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        if USER_OWNED.contains(&path.as_str()) {
            continue;
        }
        match reference.get(key) {
            None => out.push(path),
            Some(r) => {
                if let (Some(d), Some(r)) = (value.as_table(), r.as_table()) {
                    collect_unknown(d, r, &path, out);
                }
            }
        }
    }
}

/// A rough summary for the installer to print.
#[must_use]
pub fn describe(added: &Added) -> BTreeMap<String, Vec<String>> {
    let mut by_table: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in &added.keys {
        let (table, leaf) = key.rsplit_once('.').unwrap_or(("", key.as_str()));
        by_table
            .entry(table.to_string())
            .or_default()
            .push(leaf.to_string());
    }
    by_table
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFERENCE: &str = r#"
port = 1971

[wol]
macs = []
port = 9

[clipboard]
send = true
receive = true

[session]
lock_command = []
unlock_command = []

[commands]
"#;

    #[test]
    fn a_whole_table_the_file_lacks_is_added() {
        // Reported as the table, not as each key inside it: it arrives as one
        // unit, and listing five children of something that did not exist reads
        // as more churn than happened.
        let existing = "port = 1971\n\n[clipboard]\nsend = true\nreceive = true\n";
        let (text, added) = reconcile(existing, REFERENCE).unwrap();
        assert!(added.keys.contains(&"session".to_string()));
        assert!(added.keys.contains(&"wol".to_string()));
        assert!(text.contains("unlock_command"));
    }

    #[test]
    fn a_single_setting_added_to_a_table_that_already_exists() {
        // The case that actually happens when a version adds one option: the
        // table is there, one key inside it is not.
        let existing = "\
port = 1971

[session]
# I set this up when unlocking stopped working
lock_command = [\"my-locker\"]
";
        let (text, added) = reconcile(existing, REFERENCE).unwrap();
        assert!(
            added.keys.contains(&"session.unlock_command".to_string()),
            "the missing key is named in full: {:?}",
            added.keys
        );
        assert!(!added.keys.contains(&"session.lock_command".to_string()));

        let parsed: DocumentMut = text.parse().unwrap();
        assert_eq!(
            parsed["session"]["lock_command"][0].as_str(),
            Some("my-locker"),
            "what was already there is untouched"
        );
        assert!(text.contains("# I set this up when unlocking stopped working"));
    }

    #[test]
    fn a_value_the_user_set_is_never_reset() {
        // The failure this prevents is quiet and long-lived: an update that
        // "helpfully" restored a default would undo a deliberate choice, and
        // nothing would say so.
        let existing = "port = 4242\n\n[clipboard]\nsend = false\nreceive = false\n";
        let (text, _) = reconcile(existing, REFERENCE).unwrap();
        let parsed: DocumentMut = text.parse().unwrap();
        assert_eq!(parsed["port"].as_integer(), Some(4242));
        assert_eq!(parsed["clipboard"]["send"].as_bool(), Some(false));
        assert_eq!(parsed["clipboard"]["receive"].as_bool(), Some(false));
    }

    #[test]
    fn comments_and_layout_survive() {
        // The reason toml_edit is here rather than a parse-and-reserialise.
        let existing = "\
# my notes
port = 1971 # deliberately not the default

[clipboard]
# I do not want my clipboard leaving this machine
send = false
receive = true
";
        let (text, added) = reconcile(existing, REFERENCE).unwrap();
        assert!(!added.is_empty(), "there was something to add");
        assert!(text.contains("# my notes"));
        assert!(text.contains("# deliberately not the default"));
        assert!(text.contains("# I do not want my clipboard leaving this machine"));
    }

    #[test]
    fn a_user_owned_table_is_not_filled_in() {
        let existing = "[commands]\n\n[commands.mine]\nname = \"Mine\"\n";
        let (text, _) = reconcile(existing, REFERENCE).unwrap();
        assert!(text.contains("[commands.mine]"));
        assert!(
            !text.contains("program = \"\""),
            "no skeleton command invented"
        );
    }

    #[test]
    fn an_up_to_date_file_is_returned_untouched() {
        // Byte-identical, so an installer can skip the write and say nothing.
        let (text, added) = reconcile(REFERENCE, REFERENCE).unwrap();
        assert!(added.is_empty());
        assert_eq!(text, REFERENCE);
    }

    #[test]
    fn running_it_twice_changes_nothing_the_second_time() {
        let existing = "port = 1971\n";
        let (once, first) = reconcile(existing, REFERENCE).unwrap();
        assert!(!first.is_empty());
        let (twice, second) = reconcile(&once, REFERENCE).unwrap();
        assert!(second.is_empty(), "an update must be idempotent");
        assert_eq!(once, twice);
    }

    #[test]
    fn a_setting_we_do_not_know_is_reported_not_removed() {
        let existing = "port = 1971\nspeling_mistake = 3\n";
        let (text, _) = reconcile(existing, REFERENCE).unwrap();
        assert!(text.contains("speling_mistake"), "never deleted");
        assert_eq!(unknown_keys(existing, REFERENCE), vec!["speling_mistake"]);
    }

    #[test]
    fn malformed_toml_is_an_error_not_a_silent_overwrite() {
        assert!(reconcile("port = = 1971", REFERENCE).is_err());
    }
}
