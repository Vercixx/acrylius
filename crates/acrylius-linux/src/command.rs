//! Running one of a fixed set of commands.
//!
//! Nothing a peer sends reaches a shell, an argument, or a path. A peer picks an
//! id from a list this machine published; everything about what that id means
//! lives in this machine's own configuration.
//!
//! The runner adds three limits that exist because a command someone triggers
//! from a phone is a command nobody is watching: an absolute path so `PATH`
//! cannot be redirected, a timeout so a hung process does not accumulate, and an
//! output cap so a chatty one cannot exhaust memory.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use acrylius_core::plugins::command::{CommandEntry, Exited};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_OUTPUT_CAP: usize = 64 * 1024;

/// One entry in the machine's own configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    /// Absolute path. A bare name would be resolved through `PATH`, which a
    /// user's shell configuration can change.
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub needs_confirm: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct CommandCatalog {
    entries: BTreeMap<String, CommandSpec>,
}

impl CommandCatalog {
    #[must_use]
    pub fn new(entries: BTreeMap<String, CommandSpec>) -> Self {
        Self { entries }
    }

    /// What to publish to peers. Ids and names only: a peer never learns what a
    /// command actually runs, because it has no use for that and no business
    /// with it.
    #[must_use]
    pub fn manifest(&self) -> Vec<CommandEntry> {
        self.entries
            .iter()
            .map(|(id, spec)| CommandEntry {
                id: id.clone(),
                name: spec.name.clone(),
                needs_confirm: spec.needs_confirm,
            })
            .collect()
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&CommandSpec> {
        self.entries.get(id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Reject a configuration that cannot be run safely, at load time rather
    /// than at the moment someone tries to use it from a phone.
    pub fn validate(&self) -> Result<(), String> {
        for (id, spec) in &self.entries {
            if !spec.program.starts_with('/') {
                return Err(format!(
                    "command {id:?} has a relative program {:?}; use an absolute path",
                    spec.program
                ));
            }
            if id.is_empty() {
                return Err("a command id may not be empty".to_string());
            }
        }
        Ok(())
    }
}

pub async fn run(spec: &CommandSpec, run_id: u32) -> anyhow::Result<Exited> {
    // argv, never a shell string. There is no interpolation anywhere in this
    // path, so there is nothing to quote and nothing to escape.
    let mut child = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let mut stdout = child.stdout.take();
    let timeout = spec
        .timeout_secs
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs);

    // Draining and waiting go under ONE timeout, together.
    //
    // Reading to EOF first and only then waiting looks equivalent and is not: a
    // process that never writes and never exits holds the read open for as long
    // as it lives, so the timeout below would not be reached until the command
    // had already finished. Everything that can block belongs inside.
    let outcome = tokio::time::timeout(timeout, async {
        let mut out = Vec::new();
        let mut truncated = false;
        if let Some(pipe) = stdout.as_mut() {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = pipe.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                if out.len() + n > DEFAULT_OUTPUT_CAP {
                    truncated = true;
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
        }
        let status = child.wait().await;
        (status, truncated)
    })
    .await;

    match outcome {
        Ok((status, truncated)) => Ok(Exited {
            run_id,
            code: status?.code().unwrap_or(-1),
            truncated,
        }),
        Err(_) => {
            // Kill rather than leave it. Nobody is watching this process, and a
            // stuck one would sit there until the daemon restarts.
            //
            // `child` was moved into the future above, which has now been
            // dropped, and `kill_on_drop` means the drop did the killing.
            tracing::warn!(program = %spec.program, ?timeout, "command timed out and was killed");
            Ok(Exited {
                run_id,
                code: -1,
                truncated: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(program: &str) -> CommandSpec {
        CommandSpec {
            name: "test".to_string(),
            program: program.to_string(),
            args: Vec::new(),
            needs_confirm: false,
            timeout_secs: None,
        }
    }

    fn catalog(pairs: &[(&str, CommandSpec)]) -> CommandCatalog {
        CommandCatalog::new(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn a_relative_program_is_refused_at_load_time() {
        // Better here than at the moment somebody taps a button on a phone.
        let c = catalog(&[("x", spec("true"))]);
        assert!(c.validate().is_err());
    }

    #[test]
    fn an_absolute_program_is_accepted() {
        assert!(catalog(&[("x", spec("/bin/true"))]).validate().is_ok());
    }

    #[test]
    fn the_manifest_does_not_leak_what_a_command_runs() {
        let c = catalog(&[("screenshot", spec("/usr/bin/grim"))]);
        let m = c.manifest();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].id, "screenshot");
        let rendered = format!("{m:?}");
        assert!(
            !rendered.contains("grim"),
            "a peer has no business knowing the program"
        );
    }

    #[tokio::test]
    async fn a_command_reports_its_exit_code() {
        let e = run(&spec("/bin/true"), 1).await.unwrap();
        assert_eq!(e.code, 0);
        assert!(!e.truncated);

        let e = run(&spec("/bin/false"), 2).await.unwrap();
        assert_eq!(e.code, 1);
    }

    #[tokio::test]
    async fn a_hung_command_is_killed_rather_than_left() {
        let mut s = spec("/bin/sleep");
        s.args = vec!["60".to_string()];
        s.timeout_secs = Some(1);
        let started = std::time::Instant::now();
        let e = run(&s, 3).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it should not have waited"
        );
        assert_eq!(e.code, -1);
        assert!(e.truncated);
    }

    #[tokio::test]
    async fn output_is_capped_and_says_so() {
        let mut s = spec("/bin/sh");
        // A shell here is the test's own doing, not something a peer can reach.
        s.args = vec![
            "-c".to_string(),
            format!("yes x | head -c {}", DEFAULT_OUTPUT_CAP * 2),
        ];
        let e = run(&s, 4).await.unwrap();
        assert!(
            e.truncated,
            "a chatty command must be reported as truncated"
        );
    }
}
