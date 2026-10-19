# 21 — Surviving a daemon restart

Issues [#65](https://github.com/alehatsman/teleport/issues/65) ("redeploy/restart
kills live sessions with no handoff") and
[#81](https://github.com/alehatsman/teleport/issues/81) ("daemon: in-place `exec`
hot restart") are the same problem stated twice: shipping a new `teleportd`
marks every running session `lost` / `daemon_restart`.

**#81's proposed mechanism is superseded.** Its premise — that `teleportd` holds
the PTY master fds, so only an `exec(2)` over the same process can keep them open
— stopped being true when the anchor path landed. On that path no PTY master
exists inside `teleportd` at all, and a restart is already survivable. This is
demonstrated by a test, not asserted
(`daemon/tests/anchor_http.rs:589`, `an_agent_survives_teleport_restarting_under_it`).

What remains of #65 is **not** fd preservation. It is three smaller things: a
default that is still off, state that is re-derived imperfectly across the
restart, and a second process (`anchord`) that has the original problem.

This doc specifies that remainder. It proposes no implementation and closes no
issue on its own.

Read [20-retiring-the-in-process-pty.md](20-retiring-the-in-process-pty.md)
first — its [Ordering](20-retiring-the-in-process-pty.md#ordering) section is the
schedule this work hangs off, and nothing here reorders it.

## The claim, and the evidence for it

`exec(2)` would exist to keep the PTY master fd alive across a new image. On the
anchor path `teleportd` never opens one:

| What | In-process | Anchor |
|---|---|---|
| Who calls `openpty` | `teleportd` (`daemon/src/pty.rs`) | `anchord`, over a unix socket (`daemon/src/anchor_client.rs:148`, `create`) |
| What `teleportd` holds | PTY master, child handle, reader thread | a `UnixStream` per request (`anchor_client.rs:29`) |
| What a `teleportd` exit closes | every master → `SIGHUP` → the foreground process group dies | sockets |
| Session after the exit | `lost` / `daemon_restart` | still `running` |

The daemon advertises the difference rather than leaving callers to guess:
`sessions_survive_restart` is pushed onto the capability list exactly when an
anchor client exists (`daemon/src/api.rs:570`, `:577-583`), and the updater keys
off that one field (`desktop/src-tauri/src/updater.rs:37-38`,
`may_restart_now(running, survive) = survive || running == 0`). The capability is
read from fact — `state.anchor.is_some()` — not from the config flag that usually
produces one (`api.rs:573-576`).

[01-architecture.md#the-crash-boundary](01-architecture.md#the-crash-boundary)
already states this in its table, and
[08-packaging.md](08-packaging.md#updates-must-not-kill-sessions) already states
the updater rule. Neither needs changing. **#81 asks for a mechanism that would
be redundant with a shipped one**, at a cost — a second image-loading path, a
rollback story with no safe failure mode, an fd-inheritance protocol — that the
anchor path does not charge.

### What `exec(2)` would still buy, and whether it is worth it

Two things, both small:

1. **The listener fd.** An `exec` keeps `127.0.0.1:7337` bound, so no client
   sees `ECONNREFUSED` during the swap. Today there is a window. Clients already
   treat a drop as normal and reconnect with backoff
   ([04-api-protocol.md#keepalive-and-reconnection](04-api-protocol.md#keepalive-and-reconnection)),
   so the window costs a reconnect, not a session.
2. **In-process sessions.** On the default path (`use_anchor = false`,
   `daemon/src/config.rs:140`) `exec` would be the only thing that saves them.
   But the plan of record is to **delete** that path, not to make it survivable
   ([20](20-retiring-the-in-process-pty.md#goal)). Building hot restart for a
   layer scheduled for deletion is work with a known expiry date.

Neither justifies the mechanism. **Recommendation: close #81 as superseded, and
re-scope #65 onto the three items below.** That is a judgment call with a visible
blast radius, so it is a recommendation and not a decision — see
[Open questions](#open-questions), Q1.

## Goal

A `teleportd` upgrade on a developer's own machine does not interrupt a running
agent, and the state a client sees after the restart is the state it saw before
it — not merely a session that is technically alive.

Success is the existing M5 exit criterion, unchanged: *update teleport while
agents are running, and they survive.*

## Scope

In scope:

- **S1** — the prerequisites that make survival the default rather than an
  opt-in flag. These are [20](20-retiring-the-in-process-pty.md#ordering) steps
  3–5; this doc adds no new ones.
- **S2** — the state `teleportd` loses across its own restart on the anchor path,
  enumerated and each given a decision: re-derive, persist, or accept.
- **S3** — `anchord`'s own restart, which today has exactly the problem #65
  describes, one process down.

Out of scope:

- `exec(2)`, fd inheritance, `CLOEXEC` juggling, spawn-and-probe rollback, a
  `--check` mode. Superseded, above.
- A broker process per PTY. Rejected in #81 and in
  [01](01-architecture.md#if-daemon-crash-survival-ever-becomes-a-requirement);
  `anchord` is the broker, one per host instead of one per PTY.
- Restore-by-respawn (#67). Shipped, and a recovery path rather than survival.
- Host reboot. Separate axis — see [#40](#40--autostart-is-login-scoped) below.
- Crash survival. `anchord` dying still takes its ptys with it, on both paths
  ([01-architecture.md](01-architecture.md#the-crash-boundary)).

## Interfaces

No new wire surface is proposed. Everything below is existing surface, named so
the edge cases have something to refer to.

| Interface | Where | What it already promises |
|---|---|---|
| `GET /api/v1/health` → `capabilities[]` | `api.rs:577-583` | contains `sessions_survive_restart` iff sessions live in `anchord` |
| `GET /api/v1/health` → `sessions_running` | `api.rs:639-667` | `null`, never `0`, when `anchord` cannot be reached |
| `GET /stream?after=<offset>` | `ws.rs:151-177` → `ws_anchor::run_anchor` | replay from a byte offset; a reconnect is a normal event |
| `AnchorClient::list(owned_by(owner))` | `anchor_client.rs:165`, `anchor_bridge::owned_by` | the session list after a restart, from `anchord` |
| `WatchRegistry` | `anchor_watch.rs:54` (`Derived`) | title, resume id, bell, idle — re-derived from the replayed log |
| `LeaseRegistry` | `anchor_lease.rs`, constructed at `main.rs:261` | who may type; **in-memory only** |
| `anchor_session_meta` (schema V5) | `persistence.rs` | `title` across a restart |
| label `teleport.claude_resume_id` | `anchor_watch.rs:51` | resume id across a restart, stored on `anchord` |

The one interface question this doc raises is whether the lease needs to join
the bottom three — see Q2.

## What survives the restart today, and what does not

The anchor path's answer to #81's "reconstructing session state" bullet, item by
item. This is the part worth getting right: a session that is alive but wrong is
worse than an honest `lost`.

| State | Lives in | After a `teleportd` restart |
|---|---|---|
| PTY master, child, process group | `anchord` | **survives** |
| Output log and byte offsets | `anchord` | **survives**; clients re-attach with `after` |
| Session list and state | `anchord` | **survives**; re-read via `list` (`anchor_watch.rs:149`) |
| Subscriber list | `teleportd` memory | gone, and correctly so — the sockets died with it; clients reconnect |
| `title` | `anchor_session_meta` → reloaded at `anchor_watch.rs:127-139` | **survives** |
| `claude_resume_id` | `anchord` label | **survives** |
| `last_bell_ms` | `teleportd` memory | **lost**, silently |
| Idle clock | `teleportd` memory; `last_output_ms` seeded at watcher attach (`anchor_watch.rs:217`) | **wrong for ~30 s** — every session looks freshly active after a restart (`idle_since_ms`, `anchor_watch.rs:81-82`) |
| Control lease | `LeaseRegistry`, in-memory (`main.rs:261`) | **lost** — every session comes back with no controller |
| `session_events` rows (bells, idle transitions) | never written on this path | n/a; [20, gap 2](20-retiring-the-in-process-pty.md#coverage-gaps) |

Three of those are the real content of #65 after the mechanism question is
settled. The first two are already written up as
[20's gaps 9 and 2](20-retiring-the-in-process-pty.md#coverage-gaps); the lease
is not, and is the one this doc adds.

### The lease is the sharp edge

`LeaseRegistry` is constructed empty on every start (`main.rs:261`). A restart
therefore frees every lease. The consequences are asymmetric and worth stating
plainly, because "lease lost" sounds harmless and is not in one direction:

- **Benign:** the controller reconnects and re-claims. Attach never preempts
  (`anchor_lease.rs:108`), so a freed lease is simply claimed by whoever asks
  first.
- **Not benign:** *whoever asks first*. Before the restart, a second browser
  watching the same session was an observer and could not type. After it, the
  two are equal and the race decides. Nothing tells the previous holder it lost
  anything — the restart is invisible to it beyond a reconnect.
- The disconnect grace that exists precisely to survive a client vanishing
  (`anchor_lease.rs:238-280`, `begin_control_grace`) does not help: the grace
  timer is a `tokio` task in the process that just exited.

Note the ordering problem this creates with the single-writer invariant
(README's invariant 3, "one PTY has exactly one input writer"): `anchord`'s own
per-connection lease is the only thing left holding it during the window, which
[20, gap 6](20-retiring-the-in-process-pty.md#coverage-gaps) already calls
weaker. Q2 asks what to do about it rather than deciding.

### `anchord`'s own restart

`anchord` is where the problem went, not where it stopped. Upgrading `anchord`
closes its ptys exactly as upgrading `teleportd` used to. The current mitigation
is "`anchord` is small and changes rarely"
([01](01-architecture.md#the-crash-boundary), anchor's `docs/07-lifecycle.md`),
which is a frequency argument, not a mechanism.

**This is where an `exec(2)` design would actually pay for itself**, because
`anchord` is the process that holds the master fds and it has no other way out.
It is also not teleport's code. Q3.

## Edge cases

| # | Case | Expected behaviour | Today |
|---|---|---|---|
| E1 | Restart with zero sessions | nothing observable | holds |
| E2 | Restart with `use_anchor = false` | sessions `lost` / `daemon_restart`, UI states it honestly | holds (`05-persistence.md#restart-recovery`) |
| E3 | Updater sees no capability | stage the binary, apply when idle; never read the absence as a promise | holds (`updater.rs:37`, `08-packaging.md`) |
| E4 | `anchord` unreachable while `/health` is polled | `sessions_running: null`, `anchor_reachable: false`; updater must not restart | holds (`api.rs:646-660`) — but see Q4: `may_restart_now` takes a `u64`, so what a `null` becomes at the call site is untested |
| E5 | Client reconnects mid-frame | reconnect with its tracked `after`; a half-sent frame is discarded whole | holds; offsets are the only cursor (README invariant 4) |
| E6 | Client's `after` exceeds the new daemon's view | `offset_ahead`, restart from `0` | holds on the in-process path; on the anchor path `offset_ahead` is raised for *every* attach failure ([20, gap 5](20-retiring-the-in-process-pty.md#coverage-gaps)) |
| E7 | Two browsers, one controller, restart | the prior controller keeps control | **does not hold** — see [the lease](#the-lease-is-the-sharp-edge) |
| E8 | Session goes idle during the restart | `idle_since_ms` reflects real last output | **does not hold** — ~30 s of false activity (`anchor_watch.rs:217`) |
| E9 | BEL rings during the restart | attention signal survives | **does not hold** — `last_bell_ms` is memory-only |
| E10 | Session exits during the restart window | the new daemon reports it exited, with its transcript | `reconcile` starts no watcher for an exited session (`anchor_watch.rs`, [20, gap 2](20-retiring-the-in-process-pty.md#coverage-gaps)) — covered for `title`/resume id, unverified for the rest |
| E11 | `anchord` restarts under `teleportd` | sessions die | true, and unmitigated — Q3 |
| E12 | Windows | restart semantics, no survival | true, and stronger than intended: **the daemon does not build on Windows at all** — see [Platform](#platform) |
| E13 | Two `teleportd`s against one `anchord` | not a supported configuration | unspecified; the owner label scopes the list (`anchor_bridge::owned_by`) but nothing enforces uniqueness. Q5 |

## Platform

#81 says "`exec` is unix-only; Windows keeps restart semantics". The situation is
worse than that and should be recorded before any platform reasoning is built on
it.

`daemon/src/anchor_client.rs:29` imports `tokio::net::UnixStream` and
`daemon/src/lib.rs:8` declares the module unconditionally, with no `cfg` gate.
CI builds the daemon on `windows-latest` (`.github/workflows/ci.yml:39,51`) and
that job currently fails with `error[E0432]: unresolved import
tokio::net::UnixStream` plus four `cannot find 'unix' in 'net'`. So on Windows
there is no anchor path, no `sessions_survive_restart`, and at present no
daemon.

This is a pre-existing breakage, not something this work introduces, and it is
not in this doc's scope to fix. It is stated here because "Windows keeps restart
semantics" implies a Windows build exists. **It should be filed as its own
issue** before `use_anchor` flips, since flipping a default that cannot compile
on a first-class platform is a decision, not an accident.

> **Superseded, 2026-10-19.** It was filed (#107) and fixed by gating the
> anchor modules on `unix` (`ca58e54`); on `feat/windows-anchor` the gate is
> gone again because the client gained a named-pipe transport arm, and the
> anchor path *compiles* on Windows. E12 is still true at runtime:
> `use_anchor` is refused on Windows until anchord can host a session there
> (anchor M6). See
> [20, Constraint 3](20-retiring-the-in-process-pty.md#status-of-constraint-3--2026-10-19).

## Validation

Each check names what would fail if the behaviour regressed.

1. **Survival, end to end.** `an_agent_survives_teleport_restarting_under_it`
   (`daemon/tests/anchor_http.rs:589`) — exists, passes, and is the gate. It
   drops the `teleportd` that spawned the agent, starts a new one on the same
   `anchord`, and requires `state == "running"` with the transcript intact from
   byte zero.
2. **The capability is read from fact.** `daemon/tests/anchor_http.rs:517`
   asserts the capability is present on the anchor path;
   `daemon/tests/http_api.rs:83-89` asserts it is absent in-process. Both
   needed — a test that only checks presence passes either way.
3. **`sessions_running` is `null`, not `0`, when `anchord` is unreachable.**
   `daemon/tests/anchor_http.rs:550`.
4. **The updater's rule.** `desktop/src-tauri/src/updater.rs:105-121` covers
   capability parsing. Not covered: `may_restart_now` when `sessions_running` is
   `null` (E4, Q4).
5. **New — the controller survives a restart.** Two clients, one in control;
   restart `teleportd`; the prior controller reconnects and still holds the
   lease, and the observer still cannot type. **This test does not exist and
   would fail today.** It is the acceptance criterion for whatever Q2 decides.
6. **New — idleness is truthful after a restart.** A session quiet for well over
   the threshold, restart, assert `idle_since_ms` is non-null immediately rather
   than after another threshold's wait. Would fail today (E8).
7. **Checklist row.** `10-testing.md:203` already carries "update the daemon with
   sessions running → refused/deferred; sessions keep running". It should gain a
   second row for the anchor path: *restart under load → sessions keep running,
   clients reconnect, the controller is unchanged.*

Steps 5 and 6 are divergences between the two paths and therefore fall under
[20's constraint 2](20-retiring-the-in-process-pty.md#ordering): they must be
closed while both paths still exist and can be compared.

## Open questions

These need a decision. None is made here.

**Q1 — Does #81 get closed as superseded, or re-scoped?** The evidence says the
`exec(2)` design solves a problem the anchor path already solved, for the one
code path scheduled for deletion. The counter-argument is the listener fd: an
`exec` is the only way to make a restart invisible rather than merely survivable.
Closing an issue is the user's call.

**Q2 — Does the control lease survive a `teleportd` restart, and how?** Three
options, decreasing in cost:

- **Persist it.** A table next to `anchor_session_meta`, reloaded like `title`
  is (`anchor_watch.rs:127-139`). Correct, and introduces a second source of
  truth for something whose whole point is to be authoritative *now* — a holder
  persisted across a restart may have gone away for good.
- **Rebuild it from reconnects.** First reconnect within a grace window re-claims
  its prior lease. Needs the holder id to come from the client, which it already
  does (`client_id`, `ws.rs:138-145`), and needs the grace window to be restarted
  on boot rather than resumed.
- **Accept it, and say so.** Document that a restart frees every lease, and let
  the UI show it. Cheapest, honest, and loses the single-writer guarantee for
  the width of the reconnect race (E7).

**Q3 — Is `anchord`'s own hot restart in scope for teleport at all?** It is
anchor's process and anchor's repo. If the answer is "anchor's problem", the
`exec(2)` design from #81 should be moved there rather than dropped — it is a
better fit for a process whose entire job is holding fds. If the answer is "not
yet", the frequency argument needs to be written down as the accepted mitigation
rather than left as a sentence in a table.

**Q4 — What does the updater do with `sessions_running: null`?**
`may_restart_now(sessions_running: u64, …)` (`updater.rs:37`) has no null case.
Whether `null` deserialises to `0` — which would read "nothing to lose" at the
exact moment the daemon cannot be asked, the failure `api.rs:644-648` exists to
prevent — is untested. Verify first; it may be a bug rather than a question.

**Q5 — Is more than one `teleportd` per `anchord` supported?** The owner label
scopes what each sees, but nothing stops two. It matters here because "restart
`teleportd`" and "run a second one briefly" are the same thing from `anchord`'s
side, and a blue/green update (#65's third idea) is exactly that.

**Q6 — Does the UI need to say anything?** Today a restart under anchor is
invisible except as a reconnect. If Q2 lands on "accept", the lease change is a
user-visible event with no notification attached.

## Relationship to the other two issues

Neither is a hot-restart problem; both arrived in the same investigation and are
recorded here so they are not lost.

### #53 — the LaunchAgent does not come back after a clean exit

**Factual, and the issue's diagnosis is correct.** The template is
`~/dotfiles/components/teleport/templates/teleportd.plist.j2` (alehatsman/dotfiles,
not this repo). It sets:

```xml
<key>KeepAlive</key>
<dict>
	<key>SuccessfulExit</key>
	<false/>
</dict>
```

`SuccessfulExit: false` means "restart only on an *unsuccessful* exit". A clean
exit leaves the job loaded and down.

`teleportd` exits cleanly on all three of its stop paths: `shutdown_signal`
selects over Ctrl-C, `SIGTERM`, and the `Notify` the HTTP route wakes
(`daemon/src/main.rs:539-570`), then `main` runs to `Ok(())` at `main.rs:285`.
So `SIGTERM` — which is what a sleep/wake cycle or a `launchctl bootout`
delivers — produces exit status 0 and launchd leaves the daemon down. That
matches the observed symptom exactly.

**The fix is one key**, in dotfiles, not here:

```xml
<key>KeepAlive</key>
<true/>
```

**But it is not purely mechanical, and that is the part the issue does not
settle.** `POST /api/v1/shutdown` is a shipped, advertised capability —
`"remote_shutdown"` in `CAPABILITIES` (`api.rs:556`), routed at `api.rs:338`,
handled at `api.rs:1622`. Under unconditional `KeepAlive`, that route stops
meaning "stop the daemon" and starts meaning "restart the daemon". Which is
wanted is a product decision:

- If the daemon is "always up behind Tailscale Serve", `KeepAlive: true` is
  right and `remote_shutdown` should be re-documented as a restart (or removed).
- If `remote_shutdown` means what its name says, the plist is right as it
  stands and the gap is documentation — the issue's own second option.

**Not decided here.** It is a one-line change in another repo with a
user-visible consequence in this one. Ask before changing it.

### #40 — autostart is login-scoped

**Partly fixed since the issue was filed.** `teleportd service install` /
`uninstall` exists: `daemon/src/main.rs:77-92` (the `Service` subcommand) and
`daemon/src/service.rs`, Linux-only by construction
(`service.rs:24`, `:230`, with explicit non-Linux arms at `:233` and `:242`
rather than a silent no-op). On Linux it writes
`~/.config/systemd/user/teleportd.service` and enables lingering, which gives
boot survival with no privilege change. That closes the issue's item 2 for
Linux and its "sharper half" — the headless box no longer depends on the Tauri
tray.

Item 3 is also done: `08-packaging.md` has a **Headless install** paragraph, and
`07-remote-access.md:66-71` now states the gap in prose — "macOS and Windows have
no headless equivalent yet: reachable from a phone there currently means *the
host is logged in*".

**What is still open is item 1, the decision itself.** macOS and Windows remain
login-scoped, deliberately: a LaunchDaemon or an unattended Task Scheduler
trigger both cross the privilege line
([06-security.md#privilege](06-security.md#privilege)), and that line is not up
for revision here. So the honest M9 promise on those two platforms is
"reachable from a phone while the host is logged in", and #40 can be closed on
that basis — or kept open to track a macOS/Windows answer that does not require
root. That is a scope call, not a code gap.

One note for whoever closes it: the issue's table cites
`desktop/src-tauri/src/autostart/*.rs` and `desktop/src-tauri/src/main.rs:264`.
Those paths are still accurate; `daemon/src/service.rs` is a parallel
implementation, deliberately not shared code — the two crates have no shared
workspace, and the rationale is written at `service.rs:17-22`.
