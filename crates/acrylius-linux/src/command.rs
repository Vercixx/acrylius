//! Running one of a fixed set of commands.
//!
//! A peer only ever picks an id from a published list; nothing it sends reaches
//! a shell, argument, or path. Each command needs an absolute path, a timeout,
//! and an output cap.

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
    /// Absolute path; a bare name would resolve through a user's `PATH`.
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

    /// What to publish to peers: ids and names only, never what a command runs.
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

    /// Reject a configuration that can't run safely, before it's used from a phone.
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

/// Read a pipe to EOF, reporting whether it carried more than `cap`. Nothing is
/// kept; the pipe must still be drained or the process cannot finish.
async fn drain<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>, cap: usize) -> bool {
    let Some(mut pipe) = pipe else { return false };
    let mut buf = vec![0u8; 8192];
    let mut seen: usize = 0;
    loop {
        let n = pipe.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            return seen > cap;
        }
        seen = seen.saturating_add(n);
    }
}

pub async fn run(spec: &CommandSpec, run_id: u32) -> anyhow::Result<Exited> {
    // argv, never a shell string; nothing to quote or escape.
    let mut child = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let timeout = spec
        .timeout_secs
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs);

    // Draining and waiting share one timeout. Waiting only after EOF would let
    // a silent, hung process block here forever before the timeout ever fires.
    let outcome = tokio::time::timeout(timeout, async {
        // Both pipes must be drained fully and concurrently: an unread pipe
        // fills its buffer and blocks the process, so it can never exit.
        let (out_more, err_more) = tokio::join!(
            drain(stdout, DEFAULT_OUTPUT_CAP),
            drain(stderr, DEFAULT_OUTPUT_CAP)
        );
        let status = child.wait().await;
        (status, out_more || err_more)
    })
    .await;

    match outcome {
        Ok((status, truncated)) => Ok(Exited {
            run_id,
            code: status?.code().unwrap_or(-1),
            truncated,
        }),
        Err(_) => {
            // `child` was dropped along with the timed-out future above;
            // `kill_on_drop` did the actual killing.
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

    #[tokio::test]
    async fn a_command_with_a_lot_to_say_on_stderr_still_finishes() {
        let mut s = spec("/bin/sh");
        // Comfortably past a 64 KiB pipe buffer, on stderr, then exit cleanly.
        s.args = vec![
            "-c".to_string(),
            "i=0; while [ $i -lt 4000 ]; do echo \
             0123456789012345678901234567890123456789012345678901234567890123 >&2; \
             i=$((i+1)); done; exit 0"
                .to_string(),
        ];
        s.timeout_secs = Some(20);
        let e = run(&s, 7).await.expect("the command runs");
        assert_eq!(e.run_id, 7);
        assert_eq!(e.code, 0, "it exited on its own rather than being killed");
        assert!(e.truncated, "and it did have more to say than we keep");
    }

    #[test]
    fn a_relative_program_is_refused_at_load_time() {
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
