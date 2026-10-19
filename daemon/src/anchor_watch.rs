//! Deriving teleport's own session signals from anchor's output stream.
//!
//! Four fields teleport's API serves have no anchor equivalent, by
//! decision — `title`, `claude_resume_id`, `last_bell_ms`, `idle_since_ms`.
//! Anchor's scope doc keeps them in clients because each is a policy about
//! what a UI shows, and a second anchor client would want different ones.
//!
//! On the in-process path the PTY reader scans every byte as it passes,
//! whether or not anyone is watching. Over a socket teleport only sees
//! bytes while it is attached — so to keep the fields truthful for a
//! session nobody has open, teleport attaches to every session it owns and
//! scans the stream itself. That is one observer connection per session,
//! which is cheap: anchor is already producing those bytes for its log, and
//! an observer never takes the lease or writes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::anchor_bridge;
use crate::anchor_client::{AnchorClient, AnchorFrame};
use crate::persistence::Db;
use crate::session::osc::{OscScanner, OscUpdate};

/// How often the supervisor reconciles watchers against anchord's session
/// list. Sessions appear when a client creates one — possibly a different
/// teleport instance or a CLI — so this is a reconcile loop rather than a
/// hook on create.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

/// How long a session must produce nothing before it counts as idle.
const IDLE_THRESHOLD_MS: i64 = 30_000;

/// Ceiling on how often a derived title reaches SQLite.
///
/// The same rate the in-process path uses, for the same reason: an agent
/// re-emits its title on every prompt, so an unthrottled write would put a
/// SQLite round-trip on a busy session's output path. Leading-edge, so the
/// first title after a quiet second lands immediately rather than waiting
/// out a window it may never survive.
const TITLE_PERSIST_INTERVAL_MS: i64 = 1000;

/// The label a `claude_resume_id` is stored under.
///
/// A label rather than a row: it is written once, in an agent's startup
/// banner, so it survives a teleport restart for free and needs nothing kept
/// in step. Anchor never interprets a label, and the `teleport.` prefix says
/// whose opinion it is.
pub const LABEL_CLAUDE_RESUME_ID: &str = "teleport.claude_resume_id";

/// What teleport derives for one session from its own output.
#[derive(Debug, Clone, Default)]
pub struct Derived {
    /// The agent's most recent terminal-title update.
    pub title: Option<String>,
    /// A Claude Code resumable-conversation id, if its output carried one.
    pub claude_resume_id: Option<String>,
    /// When a BEL was last seen. `None` if never.
    pub last_bell_ms: Option<i64>,
    /// When output last arrived, used to decide idleness.
    pub last_output_ms: i64,
    /// When the title was last written through to SQLite. Throttling state,
    /// not something the API serves.
    pub title_persisted_ms: i64,
    /// The title last written through to SQLite, so the trailing write
    /// knows whether the throttle swallowed a newer one.
    pub title_persisted: Option<String>,
    /// One past the last byte this session's watcher has scanned.
    ///
    /// A watcher dropped for falling behind is restarted by the supervisor,
    /// and restarting from 0 every time would re-read the whole log on each
    /// cycle — worst on exactly the busy sessions that cause the drop. It
    /// resumes here instead.
    pub scanned_to: u64,
}

impl Derived {
    /// When this session went quiet, or `None` if it is not idle yet.
    ///
    /// Reported as "since when has this been quiet", not "when did it cross
    /// the threshold" — the former is what a UI badge wants to show.
    pub fn idle_since_ms(&self, now_ms: i64) -> Option<i64> {
        (now_ms - self.last_output_ms >= IDLE_THRESHOLD_MS).then_some(self.last_output_ms)
    }
}

/// Everything teleport has derived, by session id.
#[derive(Debug, Default)]
pub struct WatchRegistry {
    sessions: Mutex<HashMap<String, Derived>>,
    /// Sessions a watcher is already running for, so the supervisor does
    /// not start a second one on its next pass.
    watched: Mutex<std::collections::HashSet<String>>,
}

impl WatchRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// What has been derived for `session`, if anything.
    pub fn get(&self, session: &str) -> Option<Derived> {
        self.sessions.lock().get(session).cloned()
    }

    /// Forgets a session. Called when one is purged.
    pub fn forget(&self, session: &str) {
        drop(self.sessions.lock().remove(session));
        self.watched.lock().remove(session);
    }

    fn update(&self, session: &str, f: impl FnOnce(&mut Derived)) {
        let mut sessions = self.sessions.lock();
        f(sessions.entry(session.to_owned()).or_default());
    }
}

/// Starts the supervisor that keeps a watcher on every session teleport
/// owns. Returns immediately; the work runs on the runtime.
pub fn spawn_supervisor(
    anchor: AnchorClient,
    registry: Arc<WatchRegistry>,
    owner: String,
    db: Option<Db>,
) {
    tokio::spawn(async move {
        // Titles outlive the process that derived them, so a restart starts
        // from what was stored rather than blank. Without this the fields
        // the UI shows reset to null on every daemon restart -- which is
        // exactly the bug this is here to fix.
        if let Some(db) = &db {
            match db.anchor_titles().await {
                Ok(titles) => {
                    for (id, title) in titles {
                        registry.update(&id, |d| d.title = Some(title));
                    }
                }
                Err(e) => tracing::warn!(error = %e, "reloading anchor titles failed"),
            }
        }
        loop {
            reconcile(&anchor, &registry, &owner, db.as_ref()).await;
            tokio::time::sleep(RECONCILE_INTERVAL).await;
        }
    });
}

/// Starts a watcher for every running session that does not have one.
async fn reconcile(
    anchor: &AnchorClient,
    registry: &Arc<WatchRegistry>,
    owner: &str,
    db: Option<&Db>,
) {
    let Ok(sessions) = anchor.list(&anchor_bridge::owned_by(owner)).await else {
        return; // anchord is down or restarting; try again next tick
    };

    for session in sessions {
        // A resume id is stored as a label, so it is readable without a
        // watcher -- including for an exited session, which is precisely
        // when it is useful.
        if let Some(resume) = session.labels.get(LABEL_CLAUDE_RESUME_ID) {
            registry.update(&session.id, |d| {
                d.claude_resume_id = Some(resume.clone());
            });
        }

        // Only live sessions produce anything to derive. An exited one
        // keeps whatever was derived while it ran.
        if session.state != "running" && session.state != "closing" {
            continue;
        }
        let fresh = registry.watched.lock().insert(session.id.clone());
        if fresh {
            spawn_watcher(
                anchor.clone(),
                Arc::clone(registry),
                session.id.clone(),
                db.cloned(),
            );
        }
    }
}

/// Follows one session's output, deriving signals from it until it ends.
fn spawn_watcher(anchor: AnchorClient, registry: Arc<WatchRegistry>, id: String, db: Option<Db>) {
    tokio::spawn(async move {
        // **Starting from 0 is load-bearing beyond the reason below.**
        // Replaying a running session's whole log re-derives its title and
        // resume id after a teleport restart, which is why a restart test
        // against a *running* session passes even with persistence removed.
        // Only an exited session — which never gets a watcher — actually
        // depends on the stored copies. Anyone optimising this to start at
        // the head must keep the persistence in `OscUpdate::Title` and the
        // resume label: without them, running sessions would start losing
        // their metadata too, and no existing test would say so.
        //
        // From 0 the first time. The signals worth deriving are mostly
        // emitted at startup — an agent sets its title and prints its
        // resume link in its opening banner — and the supervisor cannot
        // attach before the session has produced them. Starting at the head
        // reliably misses exactly the output that matters, which is how
        // this was first written and why the title never appeared.
        //
        // On a restart it resumes where it stopped, so a watcher that was
        // dropped for falling behind does not re-read the whole log every
        // cycle.
        let from = registry.get(&id).map_or(0, |d| d.scanned_to);

        // An observer: never takes the lease, never writes.
        let Ok(mut attach) = anchor.attach(&id, from, false).await else {
            registry.watched.lock().remove(&id);
            return;
        };

        registry.update(&id, |d| d.last_output_ms = crate::now_ms());
        let mut scanner = OscScanner::default();

        loop {
            match attach.next_frame().await {
                Ok(Some(AnchorFrame::Output { offset, bytes })) => {
                    let now = crate::now_ms();
                    let end = offset + bytes.len() as u64;
                    registry.update(&id, |d| {
                        d.last_output_ms = now;
                        d.scanned_to = d.scanned_to.max(end);
                    });

                    // Same reasoning as the in-process reader: this pass
                    // already touches every byte, so the scans ride along.
                    for update in scanner.feed(&bytes) {
                        match update {
                            OscUpdate::Title(title) => {
                                // Written through on a leading-edge throttle.
                                // Deciding here rather than in `persistence`
                                // keeps the rate next to the thing that knows
                                // how often titles arrive.
                                let persist = registry.get(&id).is_none_or(|d| {
                                    now - d.title_persisted_ms >= TITLE_PERSIST_INTERVAL_MS
                                });
                                registry.update(&id, |d| {
                                    d.title = Some(title.clone());
                                    if persist {
                                        d.title_persisted_ms = now;
                                        d.title_persisted = Some(title.clone());
                                    }
                                });
                                if persist {
                                    if let Some(db) = &db {
                                        if let Err(e) = db.note_anchor_title(&id, &title).await {
                                            tracing::warn!(session_id = %id, error = %e, "persisting a title failed");
                                        }
                                    }
                                }
                            }
                            OscUpdate::ClaudeResumeId(resume) => {
                                // Already known means already labelled: an
                                // agent re-printing its banner must not cost
                                // a label write every time.
                                let known = registry
                                    .get(&id)
                                    .is_some_and(|d| d.claude_resume_id.as_ref() == Some(&resume));
                                registry.update(&id, |d| {
                                    d.claude_resume_id = Some(resume.clone());
                                });
                                if !known {
                                    let mut set = std::collections::BTreeMap::new();
                                    drop(
                                        set.insert(
                                            LABEL_CLAUDE_RESUME_ID.to_owned(),
                                            resume.clone(),
                                        ),
                                    );
                                    if let Err(e) = anchor.label(&id, &set, &[]).await {
                                        tracing::warn!(session_id = %id, error = %e, "labelling a resume id failed");
                                    }
                                }
                            }
                        }
                    }
                    if bytes.contains(&0x07) {
                        registry.update(&id, |d| d.last_bell_ms = Some(now));
                    }
                }
                // The session ended, or this observer was dropped for
                // falling behind. Either way stop watching; the supervisor
                // starts a fresh watcher if the session is still running.
                Ok(Some(AnchorFrame::Exit { .. }) | None) | Err(_) => break,
                Ok(Some(_)) => {}
            }
        }

        // The trailing edge of the throttle. Leading-edge alone drops the last
        // title of any burst, and an *exited* session -- the one case that
        // reads the stored copy rather than re-deriving it -- keeps whatever
        // the throttle let through instead of what the agent said last.
        // Found on Windows, where conhost always emits a title of its own
        // naming the program first, so an agent's real title inside the
        // same second was never stored and a restarted teleport showed
        // `C:\WINDOWS\...\powershell.exe` for every exited session. Unix
        // has it too, for any agent that sets its title twice in a second.
        if let (Some(db), Some(derived)) = (&db, registry.get(&id)) {
            if let Some(title) = derived
                .title
                .filter(|t| derived.title_persisted.as_ref() != Some(t))
            {
                match db.note_anchor_title(&id, &title).await {
                    Ok(()) => registry.update(&id, |d| d.title_persisted = Some(title)),
                    Err(e) => {
                        tracing::warn!(session_id = %id, error = %e, "persisting the last title failed");
                    }
                }
            }
        }

        registry.watched.lock().remove(&id);
    });
}
