//! Locking and unlocking a desktop session, through logind.
//!
//! [`rank_for_lock`] and [`rank_for_unlock`] look near-identical; do not merge
//! them (see their docs). State is re-read after acting; an exit status is
//! never trusted as the result.

use std::time::Duration;

use acrylius_core::plugins::session::{SessionOutcome, SessionState};

use crate::compositor;

/// How long to wait for a session to actually report unlocked.
///
/// Lives in core next to the client's own wait budget; a mismatch here
/// previously reported a working lock as failed.
const UNLOCK_CONFIRM: Duration =
    Duration::from_millis(acrylius_core::plugins::session::UNLOCK_CONFIRM_MS);
/// Longer, because a locker has more to do on the way in.
const LOCK_CONFIRM: Duration =
    Duration::from_millis(acrylius_core::plugins::session::LOCK_CONFIRM_MS);
const CONFIRM_INTERVAL: Duration = Duration::from_millis(200);

/// What `ListSessions` returns: id, uid, user name, seat, object path.
type SessionRow = (String, u32, String, String, zbus::zvariant::OwnedObjectPath);

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    fn list_sessions(&self) -> zbus::Result<Vec<SessionRow>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
trait LogindSession {
    fn lock(&self) -> zbus::Result<()>;
    fn unlock(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn active(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "Type")]
    fn kind(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}

/// A session this daemon may act on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Candidate {
    pub id: String,
    pub kind: String,
    pub active: bool,
    /// Resolved, not the raw hint. See [`resolve_locked`].
    pub locked: bool,
}

/// Which session an unlock should target: locked first, then active, then
/// lowest id.
///
/// Must not be shared with [`rank_for_lock`]: ranking locked-first for a lock
/// request would pick an already-locked session and report false success. One
/// session hides the bug, since both rankings agree there.
#[must_use]
pub fn rank_for_unlock(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .min_by_key(|c| (!c.locked, !c.active, c.id.clone()))
}

/// Which session a lock should target: unlocked sessions first, the
/// deliberate inverse of [`rank_for_unlock`].
#[must_use]
pub fn rank_for_lock(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .min_by_key(|c| (c.locked, !c.active, c.id.clone()))
}

/// Decide whether a session is locked. A `yes` hint is trusted; a `no` hint is
/// only believed once the compositor agrees, on an active Wayland session.
pub async fn resolve_locked(hint: bool, kind: &str, active: bool) -> bool {
    if hint {
        return true;
    }
    if kind != "wayland" || !active {
        return false;
    }
    compositor::locked().await.unwrap_or(false)
}

/// How to lock and unlock, when logind's signal alone isn't enough: many
/// screen lockers ignore it, so a configured command can override it. Each is
/// an argv vector, run directly with no shell.
#[derive(Clone, Debug, Default)]
pub struct Commands {
    pub lock: Vec<String>,
    pub unlock: Vec<String>,
}

pub struct SessionEffector {
    connection: zbus::Connection,
    uid: u32,
    commands: Commands,
}

impl SessionEffector {
    pub async fn new(commands: Commands) -> anyhow::Result<Self> {
        Ok(Self {
            connection: zbus::Connection::system().await?,
            uid: crate::uid(),
            commands,
        })
    }

    /// Run a configured command if set for this direction. Returns whether
    /// one ran, not whether it worked; the session is re-read either way.
    async fn run_command(&self, want_locked: bool) -> bool {
        let argv = if want_locked {
            &self.commands.lock
        } else {
            &self.commands.unlock
        };
        let Some((program, args)) = argv.split_first() else {
            return false;
        };
        match tokio::process::Command::new(program)
            .args(args)
            .status()
            .await
        {
            Ok(status) => {
                tracing::debug!(%status, want_locked, "ran the configured session command");
            }
            Err(e) => {
                tracing::warn!(error = %e, program, "could not run the configured session command");
            }
        }
        true
    }

    /// Sessions this user could be sitting at. `Class == user` drops
    /// systemd's own `manager` session.
    async fn candidates(&self) -> anyhow::Result<Vec<Candidate>> {
        let manager = ManagerProxy::new(&self.connection).await?;
        let mut out = Vec::new();
        for (id, uid, _user, _seat, path) in manager.list_sessions().await? {
            if uid != self.uid {
                continue;
            }
            let session = LogindSessionProxy::builder(&self.connection)
                .path(path)?
                .build()
                .await?;
            let (Ok(class), Ok(kind)) = (session.class().await, session.kind().await) else {
                continue;
            };
            if class != "user" || !matches!(kind.as_str(), "wayland" | "x11") {
                continue;
            }
            let active = session.active().await.unwrap_or(false);
            let hint = session.locked_hint().await.unwrap_or(false);
            out.push(Candidate {
                id,
                locked: resolve_locked(hint, &kind, active).await,
                kind,
                active,
            });
        }
        Ok(out)
    }

    async fn proxy_for(&self, id: &str) -> anyhow::Result<LogindSessionProxy<'_>> {
        let manager = ManagerProxy::new(&self.connection).await?;
        for (sid, _uid, _user, _seat, path) in manager.list_sessions().await? {
            if sid == id {
                return Ok(LogindSessionProxy::builder(&self.connection)
                    .path(path)?
                    .build()
                    .await?);
            }
        }
        anyhow::bail!("session {id} is gone")
    }

    /// Re-read a single session's lock state.
    async fn read_locked(&self, id: &str) -> anyhow::Result<bool> {
        let s = self.proxy_for(id).await?;
        let kind = s.kind().await?;
        let active = s.active().await.unwrap_or(false);
        let hint = s.locked_hint().await.unwrap_or(false);
        Ok(resolve_locked(hint, &kind, active).await)
    }

    pub async fn query(&self) -> anyhow::Result<SessionState> {
        let candidates = self.candidates().await?;
        // For a report, describe the session a person is most likely looking at.
        let chosen = candidates
            .iter()
            .min_by_key(|c| (!c.active, c.id.clone()))
            .ok_or_else(|| anyhow::anyhow!("no graphical session for this user"))?;
        Ok(SessionState {
            locked: chosen.locked,
            session_id: chosen.id.clone(),
            kind: chosen.kind.clone(),
            active: chosen.active,
        })
    }

    pub async fn lock(&self) -> anyhow::Result<SessionOutcome> {
        self.act(true).await
    }

    pub async fn unlock(&self) -> anyhow::Result<SessionOutcome> {
        self.act(false).await
    }

    async fn act(&self, want_locked: bool) -> anyhow::Result<SessionOutcome> {
        let candidates = self.candidates().await?;
        let chosen = if want_locked {
            rank_for_lock(&candidates)
        } else {
            rank_for_unlock(&candidates)
        }
        .ok_or_else(|| anyhow::anyhow!("no graphical session for this user"))?
        .clone();

        let was_locked = chosen.locked;
        if was_locked == want_locked {
            // Already where it should be; idempotent, nothing sent or waited on.
            return Ok(SessionOutcome {
                was_locked,
                locked: was_locked,
                session_id: chosen.id,
            });
        }

        // A configured command replaces the logind call, not joins it: it's
        // known to work where the signal is ignored.
        if !self.run_command(want_locked).await {
            let proxy = self.proxy_for(&chosen.id).await?;
            if want_locked {
                proxy.lock().await?;
            } else {
                proxy.unlock().await?;
            }
        }

        // Never trust the call's return; read the state back, since acting on
        // the signal is optional for the locker.
        let deadline = if want_locked {
            LOCK_CONFIRM
        } else {
            UNLOCK_CONFIRM
        };
        let locked = self.confirm(&chosen.id, want_locked, deadline).await;
        Ok(SessionOutcome {
            was_locked,
            locked,
            session_id: chosen.id,
        })
    }

    async fn confirm(&self, id: &str, want: bool, within: Duration) -> bool {
        let start = std::time::Instant::now();
        let mut last = !want;
        while start.elapsed() < within {
            match self.read_locked(id).await {
                Ok(now) => {
                    last = now;
                    if now == want {
                        return now;
                    }
                }
                Err(e) => tracing::debug!(error = %e, "could not re-read session state"),
            }
            tokio::time::sleep(CONFIRM_INTERVAL).await;
        }
        last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, locked: bool, active: bool) -> Candidate {
        Candidate {
            id: id.to_string(),
            kind: "wayland".to_string(),
            active,
            locked,
        }
    }

    #[test]
    fn the_two_rankings_disagree_and_that_is_the_point() {
        // Sharing one ranking would make lock target an already-locked session.
        let sessions = vec![c("1", true, true), c("2", false, true)];
        assert_eq!(
            rank_for_unlock(&sessions).unwrap().id,
            "1",
            "unlock wants the locked one"
        );
        assert_eq!(
            rank_for_lock(&sessions).unwrap().id,
            "2",
            "lock wants the unlocked one"
        );
    }

    #[test]
    fn one_session_hides_the_difference() {
        // With one session both rankings agree; the bug above is invisible here.
        let sessions = vec![c("1", false, true)];
        assert_eq!(rank_for_unlock(&sessions).unwrap().id, "1");
        assert_eq!(rank_for_lock(&sessions).unwrap().id, "1");
    }

    #[test]
    fn an_active_session_beats_an_inactive_one_of_the_same_state() {
        let sessions = vec![c("5", false, false), c("9", false, true)];
        assert_eq!(rank_for_lock(&sessions).unwrap().id, "9");
    }

    #[test]
    fn ties_break_on_the_lowest_id_so_the_choice_is_stable() {
        let sessions = vec![c("7", false, true), c("3", false, true)];
        assert_eq!(rank_for_lock(&sessions).unwrap().id, "3");
        assert_eq!(rank_for_lock(&sessions).unwrap().id, "3");
    }

    #[test]
    fn no_sessions_means_no_choice() {
        assert!(rank_for_lock(&[]).is_none());
        assert!(rank_for_unlock(&[]).is_none());
    }

    #[tokio::test]
    async fn a_hint_of_yes_is_never_second_guessed() {
        // A locker that maintains the hint at all tells the truth when it says locked.
        assert!(resolve_locked(true, "wayland", true).await);
        assert!(resolve_locked(true, "x11", false).await);
    }

    #[tokio::test]
    async fn a_hint_of_no_on_x11_is_taken_at_face_value() {
        // The probe only knows about Wayland; there's nothing to escalate to on x11.
        assert!(!resolve_locked(false, "x11", true).await);
    }

    #[tokio::test]
    async fn an_inactive_session_is_not_probed() {
        assert!(!resolve_locked(false, "wayland", false).await);
    }
}
