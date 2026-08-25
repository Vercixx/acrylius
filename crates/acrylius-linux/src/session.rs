//! Locking and unlocking a desktop session, through logind.
//!
//! `zbus` rather than shelling out to `loginctl`, which the previous project did
//! only because Python had no comfortable D-Bus story. Speaking the bus directly
//! also gets `PropertiesChanged` on `LockedHint`, which turns a polling loop into
//! events.
//!
//! Two things here look like duplication and are not:
//!
//! * [`rank_for_lock`] and [`rank_for_unlock`] are near-identical and **must not
//!   be merged**. See their documentation.
//! * The lock state is resolved twice, once before acting and once after. The
//!   second read is the result. An exit status is not.

use std::time::Duration;

use acrylius_core::plugins::session::{SessionOutcome, SessionState};

use crate::compositor;

/// How long to wait for a session to actually report unlocked.
const UNLOCK_CONFIRM: Duration = Duration::from_secs(5);
/// Longer, because a locker has more to do on the way in.
const LOCK_CONFIRM: Duration = Duration::from_secs(8);
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

/// Which session an **unlock** should target.
///
/// Locked sessions first, then active ones, then the lowest id.
///
/// This must not be shared with [`rank_for_lock`], and the reason is written out
/// because the bug it prevents is invisible on a normal desktop. Ranking locked
/// sessions first is right for unlock and wrong for lock: a lock request would
/// pick a session that is already locked, do nothing, and report success. With
/// one session both rankings choose the same thing, so the mistake only appears
/// on a machine that has two.
#[must_use]
pub fn rank_for_unlock(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .min_by_key(|c| (!c.locked, !c.active, c.id.clone()))
}

/// Which session a **lock** should target. Unlocked sessions first.
///
/// The inverse of [`rank_for_unlock`], deliberately.
#[must_use]
pub fn rank_for_lock(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .min_by_key(|c| (c.locked, !c.active, c.id.clone()))
}

/// Decide whether a session is locked.
///
/// A hint of `yes` is trusted. A hint of `no` is only believed once the
/// compositor agrees, and only on an active Wayland session where a probe means
/// anything. A compositor with no opinion leaves the hint standing, so a failed
/// probe never turns into "unlocked".
pub async fn resolve_locked(hint: bool, kind: &str, active: bool) -> bool {
    if hint {
        return true;
    }
    if kind != "wayland" || !active {
        return false;
    }
    compositor::locked().await.unwrap_or(false)
}

pub struct SessionEffector {
    connection: zbus::Connection,
    uid: u32,
}

impl SessionEffector {
    pub async fn new() -> anyhow::Result<Self> {
        Ok(Self {
            connection: zbus::Connection::system().await?,
            uid: crate::uid(),
        })
    }

    /// Sessions owned by this user that a person could actually be sitting at.
    ///
    /// `Class == user` drops systemd's own `manager` session, which otherwise
    /// looks like a candidate and can never be locked.
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
            // Already where it should be. Idempotent, and cheap: no signal is
            // sent and nothing is waited for.
            return Ok(SessionOutcome {
                was_locked,
                locked: was_locked,
                session_id: chosen.id,
            });
        }

        let proxy = self.proxy_for(&chosen.id).await?;
        if want_locked {
            proxy.lock().await?;
        } else {
            proxy.unlock().await?;
        }

        // logind only emits a signal. Whether anything acts on it is up to the
        // screen locker, and several do not. So the answer comes from reading
        // the state back, never from the call returning Ok.
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
        // The bug this prevents: sharing one ranking makes a lock request
        // target the already-locked session, do nothing, and report success.
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
        // Why the comment on those two functions has to exist: with a single
        // session both rankings agree, so the mistake is invisible here.
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
        // Trusting `yes` costs nothing: a locker that maintains the hint at all
        // is telling the truth when it says locked.
        assert!(resolve_locked(true, "wayland", true).await);
        assert!(resolve_locked(true, "x11", false).await);
    }

    #[tokio::test]
    async fn a_hint_of_no_on_x11_is_taken_at_face_value() {
        // The probe only knows about Wayland compositors, so there is nothing
        // to escalate to.
        assert!(!resolve_locked(false, "x11", true).await);
    }

    #[tokio::test]
    async fn an_inactive_session_is_not_probed() {
        assert!(!resolve_locked(false, "wayland", false).await);
    }
}
