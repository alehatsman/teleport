//! Update staging (docs/08-packaging.md#updates-must-not-kill-sessions,
//! docs/11-mvp-plan.md's "updater refuses to restart under load" gate item).
//!
//! **Scaffold only.** `tauri-plugin-updater` is wired into `main.rs` so the
//! dependency and its config shape exist, but its `endpoints`/`pubkey` are
//! empty in `tauri.conf.json` (`plugins.updater.active: false`) -- there is
//! no signed update artifact to point it at yet (signing is an external
//! prerequisite this milestone doesn't resolve, docs/11-mvp-plan.md#m10).
//! What *is* implemented is the one rule that matters regardless of
//! delivery mechanism: [`may_restart_now`].

/// The updater's core safety rule: **never restart the daemon while a
/// restart would kill running sessions.**
///
/// Two things decide that, and they are not the same thing:
///
/// - `sessions_running` -- how much there is to lose.
/// - `sessions_survive_restart` -- whether restarting loses it. True when
///   the daemon advertises the `sessions_survive_restart` capability on
///   `/health`, which it does when sessions live in `anchord` rather than
///   in the daemon process. Restarting that daemon drops some websockets
///   and the clients reconnect by offset; the ptys never notice.
///
/// The second is the entire reason anchor exists. Before it, shipping a
/// release meant killing every running agent, so the updater had to refuse
/// and wait for an idle moment that a long-running agent never provides.
///
/// Pure and synchronous on purpose -- whatever eventually calls the real
/// update-apply step (staged binary swap, then daemon restart) gates on
/// this first, with no other path to a restart.
///
/// Not called from `main.rs` yet -- there is no update-apply flow to gate
/// until signing exists (see this module's top comment). Kept here, tested,
/// and exported now rather than written later under time pressure once
/// signing lands and turns "add the safety check" into a rushed afterthought.
#[allow(dead_code)]
pub fn may_restart_now(sessions_running: u64, sessions_survive_restart: bool) -> bool {
    sessions_survive_restart || sessions_running == 0
}

/// If an update can't apply immediately, this is the message the "update
/// now" flow shows -- reusing the tray-quit confirmation's wording rather
/// than inventing a second copy of "you are about to lose N sessions"
/// (docs/11-mvp-plan.md#m10 edge cases).
#[allow(dead_code)]
pub fn blocked_reason(sessions_running: u64, sessions_survive_restart: bool) -> Option<String> {
    (!may_restart_now(sessions_running, sessions_survive_restart)).then(|| {
        format!(
            "{sessions_running} session(s) are still running. Updating now will stop them. \
             Teleport will otherwise apply the update the next time no sessions are running."
        )
    })
}

/// Whether a `/health` body says this daemon's sessions outlive a restart.
///
/// Absent means no: an older daemon, and one running sessions in-process,
/// answer the same way and both must be read as "a restart kills them".
/// Defaulting the other way would turn a missing field into data loss.
#[allow(dead_code)]
pub fn sessions_survive_restart(health: &serde_json::Value) -> bool {
    health
        .get("capabilities")
        .and_then(|c| c.as_array())
        .is_some_and(|caps| {
            caps.iter()
                .any(|c| c.as_str() == Some("sessions_survive_restart"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn restart_allowed_only_with_no_sessions() {
        assert!(may_restart_now(0, false));
        assert!(!may_restart_now(1, false));
        assert!(!may_restart_now(7, false));
    }

    /// The payoff: with sessions in anchord, a restart is safe no matter
    /// how many agents are running. This is the behaviour the milestone
    /// exists to deliver, so it is asserted rather than assumed.
    #[test]
    fn restart_is_allowed_under_load_when_sessions_outlive_the_daemon() {
        assert!(may_restart_now(0, true));
        assert!(may_restart_now(1, true));
        assert!(may_restart_now(50, true));
    }

    #[test]
    fn blocked_reason_only_when_blocked() {
        assert!(blocked_reason(0, false).is_none());
        assert!(blocked_reason(3, false).unwrap().contains('3'));
        assert!(
            blocked_reason(3, true).is_none(),
            "nothing is lost, so there is nothing to warn about"
        );
    }

    #[test]
    fn the_capability_is_read_from_health() {
        assert!(sessions_survive_restart(&json!({
            "capabilities": ["sessions", "sessions_survive_restart"]
        })));
        assert!(!sessions_survive_restart(&json!({
            "capabilities": ["sessions"]
        })));
    }

    /// A daemon too old to advertise anything, or a malformed body, must
    /// read as "a restart kills sessions" -- the safe answer.
    #[test]
    fn a_missing_capability_list_is_not_a_promise() {
        assert!(!sessions_survive_restart(&json!({})));
        assert!(!sessions_survive_restart(
            &json!({ "capabilities": "nonsense" })
        ));
        assert!(!sessions_survive_restart(&json!(null)));
    }
}
