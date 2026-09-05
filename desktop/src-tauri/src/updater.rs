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

/// The updater's core safety rule: **never restart the daemon while
/// sessions are running.** Stage the new binary and apply it on the next
/// start when `sessions_running == 0` instead. Pure and synchronous on
/// purpose -- whatever eventually calls the real update-apply step (staged
/// binary swap, then daemon restart) gates on this first, with no other
/// path to a restart.
///
/// Not called from `main.rs` yet -- there is no update-apply flow to gate
/// until signing exists (see this module's top comment). Kept here, tested,
/// and exported now rather than written later under time pressure once
/// signing lands and turns "add the safety check" into a rushed afterthought.
#[allow(dead_code)]
pub fn may_restart_now(sessions_running: u64) -> bool {
    sessions_running == 0
}

/// If an update can't apply immediately, this is the message the "update
/// now" flow shows -- reusing the tray-quit confirmation's wording rather
/// than inventing a second copy of "you are about to lose N sessions"
/// (docs/11-mvp-plan.md#m10 edge cases).
#[allow(dead_code)]
pub fn blocked_reason(sessions_running: u64) -> Option<String> {
    (!may_restart_now(sessions_running)).then(|| {
        format!(
            "{sessions_running} session(s) are still running. Updating now will stop them. \
             Teleport will otherwise apply the update the next time no sessions are running."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_allowed_only_with_no_sessions() {
        assert!(may_restart_now(0));
        assert!(!may_restart_now(1));
        assert!(!may_restart_now(7));
    }

    #[test]
    fn blocked_reason_only_when_blocked() {
        assert!(blocked_reason(0).is_none());
        assert!(blocked_reason(3).unwrap().contains('3'));
    }
}
