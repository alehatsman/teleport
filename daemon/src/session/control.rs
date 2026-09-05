//! The control-lease protocol: `mode=control`/`claim_control`, the epoch that
//! tells two connections sharing one `client_id` apart, and the disconnect
//! grace window. All of it is one `impl Session` block -- Rust doesn't
//! require an impl to live next to its type's definition, and splitting the
//! lease methods out here keeps `mod.rs` to Session's core surface. See
//! docs/04-api-protocol.md#control-lease and the parent module doc
//! (`session/mod.rs`) for the design this implements.

use std::sync::Arc;
use std::time::Duration;

use super::types::SessionEvent;
use super::Session;

impl Session {
    /// `mode=control` on attach. Grants the lease only when it is free or
    /// already held (actively or within grace) by `client_id` -- attach must
    /// never preempt (docs/04-api-protocol.md#why-attach-must-not-preempt).
    /// Returns the epoch this connection now holds (the caller must present
    /// it back to [`Self::write_if_controller`]/[`Self::release_control`]/
    /// [`Self::begin_control_grace`]), or `None` if control was not granted.
    pub fn attach_control(&self, client_id: &str, client_name: &str) -> Option<u64> {
        let mut lease = self.control.lock();
        match lease.holder.as_deref() {
            None => {
                lease.holder = Some(client_id.to_string());
                lease.holder_name = Some(client_name.to_string());
                lease.grace = false;
                lease.epoch += 1;
                Some(lease.epoch)
            }
            Some(holder) if holder == client_id => {
                lease.holder_name = Some(client_name.to_string());
                lease.grace = false;
                lease.epoch += 1;
                Some(lease.epoch)
            }
            Some(_) => None,
        }
    }

    /// `claim_control`. Always preempts, including during another holder's
    /// grace window (docs/04-api-protocol.md#disconnect-grace: "the lease is
    /// still preemptible"). Notifies the previous holder, if any and if
    /// different, via `control_revoked`. Returns the epoch this connection
    /// now holds.
    pub fn claim_control(&self, client_id: &str, client_name: &str) -> u64 {
        let (previous, epoch) = {
            let mut lease = self.control.lock();
            let previous = match (lease.holder.take(), lease.holder_name.take()) {
                (Some(holder), Some(name)) if holder != client_id => Some((holder, name)),
                _ => None,
            };
            lease.holder = Some(client_id.to_string());
            lease.holder_name = Some(client_name.to_string());
            lease.grace = false;
            lease.epoch += 1;
            (previous, lease.epoch)
        };
        if let Some((lost_by, _lost_name)) = previous {
            let _ = self.events.send(SessionEvent::ControlRevoked {
                lost_by,
                new_controller_id: client_id.to_string(),
                new_controller_name: client_name.to_string(),
            });
        }
        epoch
    }

    /// Explicit `release_control`. A no-op unless `client_id` still holds
    /// `epoch` -- a stale release from a connection that already lost the
    /// lease (superseded by a reconnect or a `claim_control`, both of which
    /// bump the epoch) must not clear whoever holds it now.
    pub fn release_control(&self, client_id: &str, epoch: u64) {
        let mut lease = self.control.lock();
        if lease.holder.as_deref() == Some(client_id) && lease.epoch == epoch {
            lease.holder = None;
            lease.holder_name = None;
            lease.grace = false;
        }
    }

    /// Whether `client_id`'s connection holding `epoch` is still the
    /// controller. Checking `epoch` alongside `client_id` is what tells
    /// apart two simultaneous connections that happen to share a
    /// `client_id` -- only the one holding the *current* epoch (the most
    /// recent `attach_control`/`claim_control` grant) counts.
    pub fn is_controller(&self, client_id: &str, epoch: u64) -> bool {
        let lease = self.control.lock();
        lease.holder.as_deref() == Some(client_id) && lease.epoch == epoch
    }

    /// Atomically checks `is_controller` and writes, holding the lease lock
    /// across both. Checking and writing as two separate calls left a window
    /// where a concurrent `claim_control` could move the lease in between,
    /// so a just-preempted connection's input could still reach the PTY (M4
    /// review). `Err(None)` means "not the controller"; `Err(Some(e))` means
    /// the write itself failed (session closing).
    pub fn write_if_controller(
        &self,
        client_id: &str,
        epoch: u64,
        bytes: &[u8],
    ) -> Result<(), Option<anyhow::Error>> {
        let lease = self.control.lock();
        if lease.holder.as_deref() != Some(client_id) || lease.epoch != epoch {
            return Err(None);
        }
        // Still holding `lease`: a concurrent `claim_control`/`attach_control`
        // blocks on the same mutex and cannot move the holder until this
        // write has gone out.
        self.write(bytes).map_err(Some)
    }

    pub fn controller_name(&self) -> Option<String> {
        self.control.lock().holder_name.clone()
    }

    /// Starts `client_id`'s disconnect grace window, if the connection
    /// holding `epoch` is still the lease holder at the moment its WS
    /// connection ends. A background task frees the lease after `grace_ms`
    /// unless the same `client_id` reclaims it first via
    /// [`attach_control`](Self::attach_control) (which bumps `epoch` and
    /// clears `grace`) -- the lease is **never** auto-granted to anyone else
    /// when the window expires (docs/04-api-protocol.md#disconnect-grace).
    pub fn begin_control_grace(self: &Arc<Self>, client_id: String, epoch: u64, grace_ms: u64) {
        {
            let mut lease = self.control.lock();
            if lease.holder.as_deref() != Some(client_id.as_str()) || lease.epoch != epoch {
                return; // already lost the lease before disconnecting; nothing to hold.
            }
            lease.grace = true;
        }
        let session = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(grace_ms)).await;
            let mut lease = session.control.lock();
            // Only free it if grace is still what's protecting this same
            // epoch -- a reconnect bumps `epoch` and clears `grace`, and a
            // `claim_control` from someone else already replaced `holder`
            // (and `epoch`) entirely.
            if lease.grace
                && lease.holder.as_deref() == Some(client_id.as_str())
                && lease.epoch == epoch
            {
                lease.holder = None;
                lease.holder_name = None;
                lease.grace = false;
            }
        });
    }
}
