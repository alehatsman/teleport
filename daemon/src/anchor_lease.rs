//! The control lease for anchor-hosted sessions.
//!
//! The session is in another process, so teleport's lease semantics live
//! here, keyed by session id. They were first written on the in-process
//! `Session` (`session/control.rs`, since deleted, docs/20) and this kept
//! them.
//!
//! **Why here and not in anchor.** Anchor's own lease identifies a holder by
//! the connection, which cannot express teleport's disconnect grace: a
//! browser reconnecting after a phone backgrounds *is* a different
//! connection, and it must get its own lease back rather than fight for it.
//! Grace is also policy — how long a client may vanish and still be the
//! controller is a product decision, and anchor's scope doc keeps policy in
//! clients. Anchor's lease then governs only "which teleport connection",
//! which for a single teleport daemon is nearly trivial.
//!
//! See anchor's `docs/06-teleport-seam.md`. If anchor later grows a
//! caller-supplied holder id, this collapses into a thin wrapper over it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::broadcast;

/// Capacity of a session's lease-change broadcast. Lease changes are rare —
/// a handful over a session's life — so a receiver that misses one has
/// lagged badly enough that resyncing on its next frame is the right fix.
const EVENT_CAPACITY: usize = 16;

/// Something every connection attached to a session should hear about.
///
/// There is no session object in this process to broadcast them, so this
/// registry -- which already has a per-session channel for the lease --
/// carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// The lease moved.
    ControlRevoked {
        /// The client that lost it, if anyone held it.
        lost_by: Option<String>,
        /// The client that holds it now.
        new_controller_id: String,
        /// That client's display name.
        new_controller_name: String,
    },
    /// The lease was released and is now free. `by` is the client that
    /// released it; `resume_hint` is the client it preempted on its way in,
    /// if any — a hint for the hand-back, never a grant
    /// (docs/04-api-protocol.md#releasing-and-handing-the-lease-back).
    ControlReleased {
        /// The client that released the lease.
        by: String,
        /// The client it preempted when it claimed, if any.
        resume_hint: Option<String>,
    },
    /// The PTY was resized. There is one effective size per session, so
    /// every attached client needs it, not just the one that asked.
    Resized {
        /// The new width.
        cols: u16,
        /// The new height.
        rows: u16,
    },
}

#[derive(Debug, Default)]
struct Lease {
    holder: Option<String>,
    holder_name: Option<String>,
    epoch: u64,
    /// True while the holder is disconnected but still inside its grace
    /// window. The lease is still preemptible during it.
    grace: bool,
    /// The client the current holder preempted when it claimed, if any.
    /// Only a later `release_control` reads it, to say who the lease should
    /// go back to. Set by `claim_control`, cleared by every other
    /// transition — the mirror of `session/control.rs`'s field of the same
    /// name, because a client cannot tell the two backends apart.
    preempted: Option<String>,
}

/// Every anchor-hosted session's lease, in one place.
#[derive(Debug, Default)]
pub struct LeaseRegistry {
    leases: Mutex<HashMap<String, Lease>>,
    events: Mutex<HashMap<String, broadcast::Sender<SessionEvent>>>,
}

impl LeaseRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribes to lease changes for one session, so a bystander learns
    /// who controls it now rather than finding out on its next reconnect.
    pub fn subscribe(&self, session: &str) -> broadcast::Receiver<SessionEvent> {
        let mut events = self.events.lock();
        events
            .entry(session.to_owned())
            .or_insert_with(|| broadcast::channel(EVENT_CAPACITY).0)
            .subscribe()
    }

    /// Grants the lease only if it is free, or already held by `client_id`
    /// — actively or within grace. **Attach never preempts**: a client that
    /// steals control out from under another is how two people end up
    /// fighting over one terminal.
    ///
    /// Returns the epoch this connection now holds, or `None` if someone
    /// else holds it.
    pub fn attach_control(&self, session: &str, client_id: &str, client_name: &str) -> Option<u64> {
        let mut leases = self.leases.lock();
        let lease = leases.entry(session.to_owned()).or_default();
        match lease.holder.as_deref() {
            Some(holder) if holder != client_id => None,
            // `preempted` survives the same-holder arm (one client resuming
            // its own lease still hands back to whoever it took it from) and
            // is cleared on a free-lease grant, which preempted nobody.
            Some(_) => {
                lease.holder_name = Some(client_name.to_owned());
                lease.grace = false;
                lease.epoch += 1;
                Some(lease.epoch)
            }
            None => {
                lease.holder = Some(client_id.to_owned());
                lease.holder_name = Some(client_name.to_owned());
                lease.grace = false;
                lease.epoch += 1;
                lease.preempted = None;
                Some(lease.epoch)
            }
        }
    }

    /// Takes the lease, preempting whoever holds it — including during
    /// another holder's grace window, which is still preemptible. Tells
    /// every other connection who holds it now.
    pub fn claim_control(&self, session: &str, client_id: &str, client_name: &str) -> u64 {
        let (lost_by, epoch) = {
            let mut leases = self.leases.lock();
            let lease = leases.entry(session.to_owned()).or_default();
            let lost_by = match lease.holder.take() {
                Some(holder) if holder != client_id => Some(holder),
                _ => None,
            };
            lease.holder = Some(client_id.to_owned());
            lease.holder_name = Some(client_name.to_owned());
            lease.grace = false;
            lease.epoch += 1;
            lease.preempted.clone_from(&lost_by);
            (lost_by, lease.epoch)
        };

        if let Some(tx) = self.events.lock().get(session) {
            // A failure here means nobody is listening, which is not a
            // problem worth propagating.
            drop(tx.send(SessionEvent::ControlRevoked {
                lost_by,
                new_controller_id: client_id.to_owned(),
                new_controller_name: client_name.to_owned(),
            }));
        }
        epoch
    }

    /// Releases the lease if `client_id` still holds it at `epoch`.
    ///
    /// The epoch check is what stops a stale release — from a connection
    /// already superseded by a reconnect or a claim, both of which bump the
    /// epoch — from clearing whoever holds it now.
    /// A release that frees the lease announces it, so the client it was
    /// taken from learns the lease is available instead of sitting in
    /// observer mode dropping the user's keystrokes.
    pub fn release_control(&self, session: &str, client_id: &str, epoch: u64) {
        let resume_hint = {
            let mut leases = self.leases.lock();
            let Some(lease) = leases.get_mut(session) else {
                return;
            };
            if lease.holder.as_deref() != Some(client_id) || lease.epoch != epoch {
                return;
            }
            lease.holder = None;
            lease.holder_name = None;
            lease.grace = false;
            lease.preempted.take()
        };
        if let Some(tx) = self.events.lock().get(session) {
            drop(tx.send(SessionEvent::ControlReleased {
                by: client_id.to_owned(),
                resume_hint,
            }));
        }
    }

    /// Whether `client_id` holding `epoch` is the current controller.
    ///
    /// Checking the epoch alongside the id is what tells two simultaneous
    /// connections sharing a `client_id` apart: only the one holding the
    /// current grant counts.
    pub fn is_controller(&self, session: &str, client_id: &str, epoch: u64) -> bool {
        let leases = self.leases.lock();
        leases
            .get(session)
            .is_some_and(|l| l.holder.as_deref() == Some(client_id) && l.epoch == epoch)
    }

    /// The current controller's display name, if any.
    pub fn controller_name(&self, session: &str) -> Option<String> {
        self.leases
            .lock()
            .get(session)
            .and_then(|l| l.holder_name.clone())
    }

    /// How many of **teleport's** clients are attached to `session`.
    ///
    /// Every proxied connection subscribes for lease events and drops its
    /// receiver when the socket ends, so the receiver count is the live
    /// attachment count without a second thing to keep in step.
    ///
    /// Deliberately not the same question anchord would answer. A `anchor`
    /// CLI attached straight to anchord is a subscriber *there* and invisible
    /// here — teleport reports the clients it is serving, which is what its
    /// own `subscribers` field has always meant.
    #[must_use]
    pub fn subscriber_count(&self, session: &str) -> usize {
        self.events
            .lock()
            .get(session)
            .map_or(0, broadcast::Sender::receiver_count)
    }

    /// Starts `client_id`'s disconnect grace window, if that connection
    /// still holds the lease as its socket ends.
    ///
    /// After `grace_ms` the lease is freed — unless the same `client_id`
    /// reclaimed it first, which bumps the epoch and clears the flag. It is
    /// **never auto-granted to anyone else** when the window expires;
    /// someone else taking it is always an explicit claim.
    ///
    /// An expiry that frees the lease announces [`SessionEvent::ControlReleased`]
    /// just as [`release_control`](Self::release_control) does — the same
    /// frame, for the same reason, because a client cannot tell a holder that
    /// handed the lease back from one that simply went away, and must not have
    /// to (docs/04-api-protocol.md#disconnect-grace).
    pub fn begin_control_grace(
        self: &Arc<Self>,
        session: &str,
        client_id: String,
        epoch: u64,
        grace_ms: u64,
    ) {
        {
            let mut leases = self.leases.lock();
            let Some(lease) = leases.get_mut(session) else {
                return;
            };
            if lease.holder.as_deref() != Some(client_id.as_str()) || lease.epoch != epoch {
                return; // already lost it before disconnecting
            }
            lease.grace = true;
        }

        let registry = Arc::clone(self);
        let session = session.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(grace_ms)).await;
            // `Some(hint)` only when this expiry is what freed the lease, so
            // a window that lapsed long after someone else took it announces
            // nothing.
            let freed = {
                let mut leases = registry.leases.lock();
                leases.get_mut(&session).and_then(|lease| {
                    // Only free it if grace is still what protects this same
                    // epoch: a reconnect bumps the epoch and clears the flag, and
                    // a claim replaced the holder outright.
                    if lease.grace
                        && lease.holder.as_deref() == Some(client_id.as_str())
                        && lease.epoch == epoch
                    {
                        lease.holder = None;
                        lease.holder_name = None;
                        lease.grace = false;
                        // An empty lease has no holder to describe, so this
                        // clears either way; it is taken rather than dropped
                        // because who that holder displaced is the hand-back
                        // hint the announcement carries. Same shape as the
                        // expiry in session/control.rs.
                        Some(lease.preempted.take())
                    } else {
                        None
                    }
                })
            };
            // `leases` is released before `events` is taken, the same order
            // every other method here uses.
            if let Some(resume_hint) = freed {
                if let Some(tx) = registry.events.lock().get(&session) {
                    drop(tx.send(SessionEvent::ControlReleased {
                        by: client_id,
                        resume_hint,
                    }));
                }
            }
        });
    }

    /// Tells every attached connection the PTY was resized.
    pub fn announce_resize(&self, session: &str, cols: u16, rows: u16) {
        if let Some(tx) = self.events.lock().get(session) {
            drop(tx.send(SessionEvent::Resized { cols, rows }));
        }
    }

    /// Forgets a session's lease and its subscribers. Called when a session
    /// is purged.
    pub fn forget(&self, session: &str) {
        drop(self.leases.lock().remove(session));
        drop(self.events.lock().remove(session));
    }

    /// Whether this registry still holds anything for `session`.
    ///
    /// Exists so a test can assert a purge actually released it. The call
    /// that does the releasing is in `api.rs`'s delete handler, not here, so
    /// a unit test of [`forget`](Self::forget) would prove the wrong half —
    /// it was the missing call site, not the function, that leaked.
    #[must_use]
    pub fn is_tracking(&self, session: &str) -> bool {
        self.leases.lock().contains_key(session) || self.events.lock().contains_key(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_never_preempts_but_claim_always_does() {
        let registry = LeaseRegistry::new();
        let first = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");

        // A second client asking politely is refused.
        assert!(registry.attach_control("s", "bob", "Bob").is_none());
        assert!(registry.is_controller("s", "alice", first));

        // Claiming always works, and alice's epoch stops being current.
        let second = registry.claim_control("s", "bob", "Bob");
        assert!(registry.is_controller("s", "bob", second));
        assert!(!registry.is_controller("s", "alice", first));
        assert_eq!(registry.controller_name("s").as_deref(), Some("Bob"));
    }

    #[test]
    fn a_stale_release_cannot_clear_the_current_holder() {
        let registry = LeaseRegistry::new();
        let alice = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");
        let bob = registry.claim_control("s", "bob", "Bob");

        // Alice's connection ends and releases with its own, now-stale epoch.
        registry.release_control("s", "alice", alice);
        assert!(
            registry.is_controller("s", "bob", bob),
            "a superseded connection must not free someone else's lease"
        );
    }

    #[test]
    fn releasing_announces_the_free_lease_and_names_the_preempted_client() {
        let registry = LeaseRegistry::new();
        let mut events = registry.subscribe("s");
        let alice = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");
        let bob = registry.claim_control("s", "bob", "Bob");
        assert_eq!(
            events.try_recv(),
            Ok(SessionEvent::ControlRevoked {
                lost_by: Some("alice".to_owned()),
                new_controller_id: "bob".to_owned(),
                new_controller_name: "Bob".to_owned(),
            })
        );

        registry.release_control("s", "bob", bob);

        assert_eq!(
            events.try_recv(),
            Ok(SessionEvent::ControlReleased {
                by: "bob".to_owned(),
                resume_hint: Some("alice".to_owned()),
            }),
            "the client bob preempted is who the lease hands back to"
        );
        assert!(registry.controller_name("s").is_none(), "freed, not moved");
        assert!(
            !registry.is_controller("s", "alice", alice),
            "a hint is not a grant -- alice still has to claim"
        );
    }

    #[test]
    fn releasing_an_unpreempted_lease_hints_nobody() {
        let registry = LeaseRegistry::new();
        let mut events = registry.subscribe("s");
        let alice = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");

        registry.release_control("s", "alice", alice);

        assert_eq!(
            events.try_recv(),
            Ok(SessionEvent::ControlReleased {
                by: "alice".to_owned(),
                resume_hint: None,
            })
        );
    }

    #[test]
    fn a_resumed_lease_still_remembers_who_it_was_taken_from() {
        let registry = LeaseRegistry::new();
        let mut events = registry.subscribe("s");
        let _alice = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");
        let _bob = registry.claim_control("s", "bob", "Bob");
        drop(events.try_recv()); // the revoke

        // Bob's socket drops and reconnects: a new grant to the same holder,
        // so the hand-back target must survive it.
        let resumed = registry
            .attach_control("s", "bob", "Bob")
            .expect("the same client resumes its own lease");
        registry.release_control("s", "bob", resumed);

        assert_eq!(
            events.try_recv(),
            Ok(SessionEvent::ControlReleased {
                by: "bob".to_owned(),
                resume_hint: Some("alice".to_owned()),
            })
        );
    }

    #[test]
    fn the_same_client_reconnecting_gets_its_lease_back() {
        let registry = LeaseRegistry::new();
        let first = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");

        // The browser drops and reconnects with the same client id: a new
        // connection, the same client. It must not have to fight for it.
        let second = registry
            .attach_control("s", "alice", "Alice")
            .expect("the same client reclaims its own lease");
        assert_ne!(first, second, "a reconnect is a new grant");
        assert!(registry.is_controller("s", "alice", second));
        assert!(!registry.is_controller("s", "alice", first));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grace_frees_the_lease_but_never_hands_it_over() {
        let registry = Arc::new(LeaseRegistry::new());
        let epoch = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");

        registry.begin_control_grace("s", "alice".to_owned(), epoch, 50);
        // Still hers during the window.
        assert!(registry.is_controller("s", "alice", epoch));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!registry.is_controller("s", "alice", epoch));
        // Freed, not transferred: nobody holds it now.
        assert!(registry.controller_name("s").is_none());
    }

    /// The bug behind #105: a lease freed by an expiring grace window is in
    /// exactly the same state as one handed back explicitly, and has to be
    /// announced the same way. Without this the client the holder preempted
    /// sits in observer mode forever, dropping every keystroke, while the
    /// session list correctly reports no controller at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn grace_expiry_announces_the_free_lease_and_hands_it_back() {
        let registry = Arc::new(LeaseRegistry::new());
        let mut events = registry.subscribe("s");
        let _alice = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");
        let bob = registry.claim_control("s", "bob", "Bob");
        drop(events.try_recv()); // the revoke

        // Bob's socket drops and never comes back.
        registry.begin_control_grace("s", "bob".to_owned(), bob, 50);
        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty),
            "opening the window changes nothing yet -- bob may still return"
        );

        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(
            events.try_recv(),
            Ok(SessionEvent::ControlReleased {
                by: "bob".to_owned(),
                resume_hint: Some("alice".to_owned()),
            }),
            "expiry frees the lease, so it must say so -- and name who it goes back to"
        );
        assert!(registry.controller_name("s").is_none(), "freed, not moved");
    }

    /// The announcement is tied to *this* expiry doing the freeing, not to
    /// the window merely elapsing. A claim already moved the lease on, and
    /// telling everyone it came free would be a lie that lands after the
    /// fact and unseats the new controller in every other client's UI.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_window_that_lapsed_after_a_claim_announces_nothing() {
        let registry = Arc::new(LeaseRegistry::new());
        let mut events = registry.subscribe("s");
        let alice = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");

        registry.begin_control_grace("s", "alice".to_owned(), alice, 50);
        let bob = registry.claim_control("s", "bob", "Bob");
        drop(events.try_recv()); // the revoke

        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty),
            "alice's dead window must not announce bob's live lease as free"
        );
        assert!(
            registry.is_controller("s", "bob", bob),
            "and must not free it either"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reclaiming_inside_the_window_survives_its_expiry() {
        let registry = Arc::new(LeaseRegistry::new());
        let first = registry
            .attach_control("s", "alice", "Alice")
            .expect("free");
        registry.begin_control_grace("s", "alice".to_owned(), first, 50);

        // She reconnects before the window closes.
        let second = registry
            .attach_control("s", "alice", "Alice")
            .expect("reclaim inside grace");

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            registry.is_controller("s", "alice", second),
            "the expiring window must not free the lease she just reclaimed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bystanders_are_told_who_controls_it_now() {
        let registry = LeaseRegistry::new();
        let mut events = registry.subscribe("s");
        let _alice = registry.attach_control("s", "alice", "Alice");

        registry.claim_control("s", "bob", "Bob");
        let event = events.try_recv().expect("a revocation was broadcast");
        assert_eq!(
            event,
            SessionEvent::ControlRevoked {
                lost_by: Some("alice".to_owned()),
                new_controller_id: "bob".to_owned(),
                new_controller_name: "Bob".to_owned(),
            }
        );
    }
}
