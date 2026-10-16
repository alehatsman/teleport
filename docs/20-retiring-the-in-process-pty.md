# 20 — Retiring the in-process PTY layer

Teleport can run a session two ways. In-process, where `SessionManager` owns a
`portable-pty` child and `ws::run` serves it; or delegated to `anchord`, where
teleport is a client of another process and `ws_anchor::run_anchor` proxies it.
The switch is one flag, `use_anchor`, default `false` (`daemon/src/config.rs:53`).

Anchor's `ROADMAP.md` M5 ends with "delete teleport's in-process PTY layer".
This doc specifies that deletion: what goes, what stays, what has to be true
first, and what the tests become.

**Nothing here is a decision to flip the default.** Flipping is a separate,
earlier step with its own prerequisites — see [Ordering](#ordering). This doc
exists so the delete, when it happens, is a bounded edit against a written
inventory rather than a discovery exercise.

Read anchor's `docs/06-teleport-seam.md` for why the seam sits at the session
layer, and `docs/01-architecture.md` for what the in-process layer is.

## Status — 2026-10-16

**Steps 4–6 of [Ordering](#ordering) are satisfied.** `anchord` is packaged and
running under its own lifecycle, `use_anchor=true` is deployed on `work_mac`, and
a release has run on the anchor path. Both M5 exit criteria were demonstrated
against the live daemon at `4912d9c`:

- a session created by the **old** teleportd (`5c5e476`) survived the upgrade to
  `4912d9c`, still running, transcript intact from byte 0, and the **new** daemon
  served it from `GET /api/v1/sessions`
- `anchord` was rebuilt and bounced underneath a running teleportd, which never
  noticed — same pid, no warnings logged, reconnected lazily, created a session
  immediately afterward

That discharges Constraint 1: the restart that proves survival is no longer the
restart that kills the thing being proved.

**Step 7 — the deletion itself — is not unblocked by that.** It acquires two
preconditions the earlier steps did not have, because deleting the in-process
layer removes the fallback that made those steps reversible. They are Constraints
3 and 4 below. Both involve machines and a second repository, so neither lands as
a consequence of merging this doc.

**One caveat on the evidence above.** The 26/26 anchor-suite run cited for step 6
is real, but it is green because `ANCHORD_BIN` was exported by hand. Run without
it, those same 26 tests print `ok` having executed nothing — they early-return,
and a returning test counts as a pass (#108, fixed for CI in `5db5fbd`). This
matters here specifically: after step 7 that suite is the **only** coverage of
the session path, so a silent skip stops being an annoyance and becomes total
blindness.

## Goal

The daemon has **one** session path. `use_anchor` is gone, `anchord` is a
requirement rather than an option, and no code in teleport spawns a PTY.

Success: the test suite passes, and no behaviour listed in
[Coverage gaps](#coverage-gaps) has regressed silently.

## Scope

In scope: deleting the in-process session layer and collapsing the seam.

Out of scope: flipping the default, packaging `anchord` as its own service,
and any change to anchor's protocol. Those gate this work; they are not part
of it.

## The seam

Sixteen sites branch on `use_anchor` or on `AppState.anchor` being `Some`.
Collapsing them is the whole structural edit.

| # | Site | Becomes |
|---|---|---|
| 1 | `config.rs:53,107,140,173` | Field deleted from `ConfigFile` and `Config`; tests at `config.rs:238-247` go. `anchor_socket` / `anchor_owner` stay. |
| 2 | `main.rs:234-243` | `let anchor = AnchorClient::new(socket);` — unconditional. Socket resolution (`main.rs:576`) stays. |
| 3 | `main.rs:248-254` | `spawn_supervisor` unconditional. |
| 4 | `main.rs:173-181` | `SessionManager` construction and `spawn_gc_task` deleted, with `log_limits` (`:168-172`) and `run_gc_pass` (`:416-503`). **`sweep_expired_auth_sessions` (`:431`) rides the GC pass and must be re-homed onto its own timer.** |
| 5 | `main.rs:275,394-414` | Idle sweep deleted; idleness moves to `anchor_watch::Derived::idle_since_ms` (`anchor_watch.rs:60`). See gap 9. |
| 6 | `main.rs:256-261` | `AppState.sessions` removed (`api.rs:49`); `anchor` becomes non-`Option` (`api.rs:56`). |
| 7 | `api.rs:580-586` | `SESSIONS_SURVIVE_RESTART` joins the `CAPABILITIES` const (`api.rs:554`). |
| 8 | `api.rs:641-670` | Keep the anchor arm (`:648-662`), including `anchor_reachable`. Drop the `SessionState` import. |
| 9 | `api.rs:741-784` | Anchor branch becomes the body; `:786-817` and the `pty::SpawnSpec` import (`:38`) go. |
| 10 | `api.rs:1004-1024` | Anchor branch becomes the body; `SessionView::from` (`:917`) and `from_row` (`:956`) go. |
| 11 | `api.rs:1056-1066` | Anchor branch becomes the body; `historical_row` (`:1085`) goes. |
| 12 | `api.rs:1134-1148` | Anchor branch becomes the body; the `cognitive_complexity` expect (`:1117`) can go. |
| 13 | `api.rs:1237-1262` | Anchor branch becomes the body; `LogReader` import (`:35`) and `ApiError::Gone` (`:170`) drop out. |
| 14 | `ws.rs:122-132` | In-process lookup deleted. |
| 15 | `ws.rs:151-196` | `run_anchor` dispatch unconditional; `ws::run` and helpers deleted. |
| 16 | `api.rs:1622-1638` | **Has no anchor branch today — it is already broken.** See gap 1. |

## What goes

Whole modules: `pty.rs` (except `login_shell_path`, **and except
`ConptyDsrProbe` at `:423-520`, which transfers to anchor — see Constraint 3**),
`session/manager.rs`,
`session/fanout.rs`, `session/replay.rs`, `session/control.rs`, the `Session`
struct and impl in `session/mod.rs:119-566`, `log.rs` in full, and `ws.rs`
except `upgrade` and `bound_attach`.

From `session/types.rs`: `Chunk`, `SessionState`, `SessionLostReason`,
`SessionMeta`, `Runtime`, `ControlLease`, and the in-process `SessionEvent`
(`:197`) — which is a different type from `anchor_lease::SessionEvent`
(`anchor_lease.rs:38`); the anchor path never touches it.

From `persistence.rs`: the session-row half — `insert_session_blocking:381`
through `note_event:498`, plus `list_sessions:527`, `get_session:517`,
`recover():866` and `RecoverySummary:197`. **The auth, passkey and pin half
stays** and is used throughout (`auth.rs`, `auth_routes.rs`,
`api.rs:1447-1499`). `AppState.db` survives — for auth and pins, not sessions.

Already dead: `persistence::sessions_root` (`persistence.rs:1254`), zero
callers. The `sessions_root` matches under `tests/` are local functions that
share the name.

**No route is deleted.** Every session route either branches or is
path-agnostic.

## What stays

These block a wholesale `session/` delete. Each has an anchor-path caller:

| Symbol | Defined | Why |
|---|---|---|
| `SessionId` + `FromStr` | `session/types.rs:15,34` | The WS-ticket store is keyed by it (`auth.rs:27,104,203`). |
| `CreateError` | `session/manager.rs:69` | Drives the status mapping via `anchor_bridge.rs:86-88`. |
| `osc::{OscScanner, OscUpdate}` | `session/osc.rs:44,55,63` | The anchor path's title derivation *is* the in-process scanner (`anchor_watch.rs:24,149`). |
| `ws::bound_attach` | `ws.rs:205` | Called by `ws_anchor.rs:103`, with its five unit tests. |
| `ws::upgrade` | `ws.rs:82` | The only `/stream` handler: Origin check, id parse, auth, then dispatch. |
| `pty::login_shell_path` | `pty.rs:184` | `presets.rs:63` calls it on both paths. |
| `api::requested_range` | `api.rs:1308` | Shared by design (`api.rs:1306`). |

Minimum surviving residue of `session/`: `SessionId`, `CreateError`, `osc.rs`.

### Looks shared, is not

- **`log.rs`** appears shared because `config.rs:20` imports two constants
  from it. Those feed `SessionManager::with_limits` only (`main.rs:168-173`).
  The whole module is in-process-only, constants included.
- **`Db::delete_session` / `gc_candidates`** (`persistence.rs:533,543`) are
  called from the GC task, which `main.rs:176` spawns **unconditionally** — so
  on the anchor path it already runs against rows nothing creates.
- **`LogReader`** (`log.rs:416`) is used at `api.rs:1280`, behind the anchor
  early-return at `:1237`.

### Looks dead, is not

`Session::subscribe` (`session/mod.rs:220`) and `log_capped_at` (`:292`) have
no `src/` callers but are live from `tests/session_catchup.rs:162`,
`tests/session_backpressure.rs:217`, `tests/session_replay.rs:351`. The `eof`
machinery (`session/mod.rs:150,522`) looks vestigial and is load-bearing for
the exit drain (`ws.rs:337,396`).

## Coverage gaps

What the anchor path does not do today. **This is the half that matters** —
each is a blocker or a known, written-down regression. Ordered by severity.

1. ~~**`POST /api/v1/ws-ticket` always 404s.**~~ **Fixed** (#100). Resolution
   now goes to whichever backend owns sessions. Both paths have tests; the
   route previously had none on either, which is how it stayed broken.

2. **Session rows and agent metadata are never persisted.** Nothing writes
   teleport's `sessions` table on the anchor path.

   **`title` and `claude_resume_id`: fixed.** They lived only in
   `WatchRegistry` memory and were lost on every teleport restart, which
   contradicted the rationale at `api.rs:950-955`. **`session_events` rows
   are still never written** — bells and idle transitions leave no history on
   this path. That half is open, and is lower severity: nothing reads those
   rows back today.

   **Narrower than it first looked, and worth recording why.** A *running*
   session recovers without any persistence: the new teleport starts a
   watcher from offset 0 and re-derives both fields from the replayed log. It
   is an **exited** session that loses them, because `reconcile` starts no
   watcher for one (`anchor_watch.rs:117`) — and that is precisely when a
   resume id is useful. The first version of the test used a running session
   and passed with the fix removed; it proved nothing. This is the
   "passes on both paths for different reasons" class, hit first-hand.

   **Decided with anchor:** the two fields split on *write frequency*, not on
   ownership.

   - **`claude_resume_id` is an anchor label** (`teleport.claude_resume_id`).
     It is emitted once, in
     an agent's startup banner (`session/manager.rs:43-46`), so the write is
     rare. A label survives a teleport restart for free, needs no table, and
     creates no second source of truth. Anchor never interprets a label and
     has a test pinning that, so `teleport.claude_resume_id` stays
     self-evidently teleport's opinion.
   - **`title` is in a teleport table** (`anchor_session_meta`, schema V5).
     It is re-emitted on every prompt
     (`session/manager.rs:349-353`) — chatty by construction, which is
     exactly why the in-process path throttles it to 1/s
     (`AGENT_META_PERSIST_INTERVAL_MS`, `session/manager.rs:41-47`). Labels
     have no rate protection and every label write is a SQLite write on
     anchor's side, so this one must not go there.

   That throttle is itself the evidence for the split: it exists because the
   title is chatty and the resume id is not.

3. ~~**`controller` and `subscribers` are hardcoded**~~ **Fixed.** Both now
   come from teleport's lease registry, which is the authority on who may
   type. `subscribers` counts teleport's own attached clients — an `anchor`
   CLI attached straight to anchord is deliberately not counted, which
   matches what the field has always meant.

4. **Replay truncation is less accurate.** In-process folds in truncation
   discovered during catch-up (`ws.rs:305`) and clamps `replay_from` past a
   capped region (`session/mod.rs:265-269`). Anchor reports only
   `bound_attach`'s pre-attach verdict and echoes `from` verbatim
   (`ws_anchor.rs:103-109,149`), so a client attaching below anchord's log cap
   is told `truncated: false`.

5. **WS close and error codes diverge.** A slow consumer closes `1013
   slow_consumer` in-process (`ws.rs:375`) but `1001 "stream ended"` on anchor
   (`ws_anchor.rs:203`). `offset_ahead` is raised for a genuine offset error
   in-process (`ws.rs:253`) but for *every* attach failure on anchor
   (`ws_anchor.rs:127`). `session_closing` has no anchor counterpart
   (`ws.rs:481` vs `ws_anchor.rs:276`). Cheap to fix, expensive to discover
   later.

6. **The lease write check is no longer atomic.** `write_if_controller`
   checks and writes under one lock (`session/control.rs:137`), closing a
   documented race (`ws.rs:469-471`). The anchor path checks
   `leases.is_controller` then awaits `send_input` (`ws_anchor.rs:273-277`).
   Anchord's per-connection lease is the backstop, which is weaker.

7. **Exit drain.** In-process waits bounded on `eof` before finalising
   (`ws.rs:385-409`); anchor relies on anchord ordering `Exit` after output
   (`ws_anchor.rs:180`). `final_offset` is `delivered_to` rather than the
   session's authoritative offset — arguably more honest, but a different
   number than in-process reports (`ws.rs:606`).

8. **Resize.** Covered for teleport's own clients
   (`ws_anchor.rs:378-385`). Not covered: the `note_size` DB write
   (`session/mod.rs:346`), and a resize by a *different* anchord client (CLI,
   workmanager) produces no `resized` frame for teleport's browsers —
   `anchor_watch` tracks output, not geometry.

   **Fixed.** Anchor made `0x05` travel both ways (anchor `77dde06`) and
   teleport consumes it. The frame carries the size anchor *applied* — after
   clamping and coalescing — so teleport's own local announce was removed
   rather than kept alongside it: broadcasting the *requested* size would
   race the authoritative frame and tell a client that asked for 5000 it had
   5000, then 1000. The originator is told too and is not filtered as an
   echo, for the same reason. The attach reply now carries `cols`/`rows`, so
   a browser starts from the real size instead of waiting for a change a
   quiet session never sends.

   Still not covered: the `note_size` DB write (`session/mod.rs:346`), which
   has no anchor equivalent and nothing reads back.

9. **Idleness diverges.** `anchor_watch.rs:60` duplicates the threshold from
   `session/mod.rs:111`. Two differences: `Derived::idle_since_ms` has no
   state guard, so an **exited** session with stale data still reports
   `idle_since_ms` (in-process returns early unless running,
   `session/mod.rs:476`); and `last_output_ms` is seeded at watcher attach
   (`anchor_watch.rs:148`), so after a restart every session looks freshly
   active for 30s.

10. **OSC derivation is attach-window-limited.** `anchor_watch` only watches
    sessions carrying this owner's label (`anchor_watch.rs:117`), and a
    watcher dropped for falling behind restarts from `scanned_to` — a title
    emitted in the gap is missed. The in-process reader scans every byte
    unconditionally (`session/manager.rs:337-372`).

11. **Log size config is inert.** `log_warn_bytes` / `log_max_bytes`
    (`config.rs:95-97`) reach only `SessionManager`. Anchord applies its own
    cap. The client-visible `log_capped_at` is covered
    (`ws_anchor.rs:152`); the operator-facing knobs are not, silently. Delete
    them or document them as anchord's.

12. ~~**Purge may not wait for exit.**~~ **Resolved — it was anchor's bug,
    fixed in anchor `00f3eaa`.** `terminate` always blocked until the child
    was reaped, but the session's *recorded state* was written by a different
    thread and nothing ordered the two. So a successful `terminate` could
    reply `closing`, and the `purge` teleport sends next could be refused
    `already_exited` — the race, in the opposite direction from the obvious
    one. Anchor's reply now waits, bounded, for the state to land.

    **Teleport adds no wait.** `api.rs:1135-1145` is correct against
    `00f3eaa` and the comment at `:1130-1133` stays. Teleport now has a
    minimum anchor version for this behaviour; against an older anchord the
    race is real.

~~Separately, purging leaks the session's lease and event channel, because
`anchor_lease::forget` has zero callers.~~ **Fixed** (#101).

## Tests

**Vanish with the code** (in-process only): `session_catchup.rs`,
`session_backpressure.rs`, `session_replay.rs`, `output_log.rs`,
`restart_recovery.rs`, `pty_primitive.rs`,
`pty_term_default.rs`. Their *subject* is the deleted layer, not a behaviour
the product still has.

**Must be rewritten against the anchor path**, because they assert
product behaviour that survives: `ws_protocol.rs`, `http_api.rs`,
`reconnect_storm.rs`, `attention_signals.rs`, `osc_title_and_resume.rs`,
`pty_login_shell.rs`. Several have anchor counterparts already
(`anchor_ws.rs`, `anchor_http.rs`) — the rewrite is mostly checking which
assertions are missing there, not writing from scratch.

**Transfers to anchor rather than vanishing or being rewritten:**
`pty_primitive_windows.rs`. It is the proof of the ConPTY startup-handshake fix
(Constraint 3), it is the only Windows PTY coverage either repo has, and anchor
needs exactly it to port. Deleting it would throw away the answer to a question
anchor is currently blocked on.

**Pass on both paths for different reasons — check these individually.**
Anchor's session flagged this class as the one that goes quietly wrong:

- Anything asserting a session *appears in a list*: in-process it comes from
  SQLite, on anchor from anchord's own list. A test that only checks presence
  passes either way while the persistence gap (2) is real.
- Anything asserting `idle_since_ms` or `title`: two implementations, two
  thresholds, divergent edge cases (gaps 9, 10).
- Anything asserting a *close* on a dropped connection: `1013` and `1001` are
  both "the socket closed" to a test that does not check the code (gap 5).

**New tests the delete needs.** Done: ws-ticket on both paths (gap 1), lease
freed on purge (#101), title and resume id surviving a restart of an exited
session (gap 2), the list agreeing with the stream about the controller
(gap 3). Still missing: close codes asserted by value (gap 5).

## Ordering

Nothing below moves until the one above it is true.

**Constraint 1 — the last step cannot be used to validate itself.** M5's exit
criterion is "update teleport while agents are running, and they survive".
The agents in question run *inside* the live `teleportd`, so the restart that
would prove survival is currently the restart that kills them. It only
becomes survivable after `anchord` is packaged with its own lifecycle **and**
the default is flipped. The order below is therefore forced, not preferred:
no amount of care makes a live restart safe before step 5.

**Constraint 2 — anything needing a comparison happens before step 7.** Gaps
4 and 5 are divergences *between* the two paths. Once the in-process path is
deleted there is nothing left to compare against, so a divergence not closed
by then is simply the new behaviour, discovered later by a user.

**Constraint 3 — anchor must have a Windows transport before the delete.**
`anchor_client` speaks `UnixStream` (`daemon/src/anchor_client.rs:29`) and
`daemon/src/lib.rs:8` declares the module with no cfg gate, so the daemon does
not build for Windows today (#107). While the in-process layer exists that is a
build bug with an obvious local fix. **Once it is deleted, the cfg gate stops
being a fix at all**: gating the module on `unix` would leave Windows with a
daemon that compiles, boots, reports healthy, and cannot host a single session.

Two honest answers existed. **Decided 2026-10-16: anchor grows a Windows
transport arm (named pipe).** "teleportd is unix-only" was considered and
rejected, so Windows keeps a session path and #107 is resolved by that arm
existing rather than by a cfg attribute.

This inverts the obvious ordering, and the inversion is the point: **#107 is not
fixed before the deletion, it is decided by it.** Landing a `#[cfg(unix)]` gate
first would be work the deletion immediately undoes, and it would manufacture
exactly the silent-healthy-but-useless failure mode Constraint 4 exists to
prevent — only delivered by an attribute instead of a missing binary.

Cost of the alternative, measured on `5db5fbd`, since "just gate it" sounds
cheap: `api.rs` carries 32 anchor-dependent lines including two `AppState`
fields threaded through the core request path (`api.rs:56`, `api.rs:60`), and
`main.rs` another 20. The gate is a cfg-split of `AppState` and ~50 handler call
sites, not a one-line attribute.

The transport seam is anchor's design to make; teleport's side is to carry both
arms behind one `AnchorClient` type and to stop assuming the socket config is a
filesystem path. **This work lives in the anchor repo and gates this one.**

#### Constraint 3 is three things, and one of them is already solved — here

Anchor's `src/pty.rs:19` defers Windows and cites teleport's **#W1** ("a
gracefully-exiting ConPTY child was never observed as exited") as the reason.
That citation is **stale, by six weeks.**

**W1 was resolved 2026-09-05** ([#9](https://github.com/alehatsman/teleport/issues/9),
closed). It was never a reaping bug. Root cause: conhost's ConPTY *startup
handshake* writes a Device Status Report / cursor-position query (`ESC[6n`) to
the pty and blocks its `ConsoleIoThread` on a reply nothing ever sent — the child
had exited, conhost was still mid-handshake. The fix is `ConptyDsrProbe`
(`daemon/src/pty.rs:423-520`), which answers that one startup query and then
deliberately stops answering, so a later `ESC[6n` from an application still gets
a real cursor position. Proven against the real `pty::spawn` path by
`daemon/tests/pty_primitive_windows.rs`; spike evidence in
`docs/15-open-questions.md`, W1.

So the Windows cost is the pipe plus a DACL story for anchor's
"filesystem-permissions-are-the-boundary" model — **not** ConPTY exit reporting.
That last one would have been the real blocker, because exit reporting is
something anchor is opinionated about by design; shipping a platform where it is
unreliable would make a core invariant false. It is already fixed.

**This creates a hazard for the delete, and it is the reason this subsection
exists.** [What goes](#what-goes) schedules `pty.rs` for deletion except
`login_shell_path`, and [Tests](#tests) lists `pty_primitive_windows.rs` as
vanishing with the code. Executed literally, **step 9 would delete
`ConptyDsrProbe` and its proof at precisely the moment anchor needs them** — the
one piece of hard-won Windows knowledge in this repo, discarded as in-process
residue on its way to becoming the thing that unblocks the port.

**Therefore:** `ConptyDsrProbe`, its module documentation, and
`pty_primitive_windows.rs` are **transferred to anchor, not deleted.** They are
exempt from [What goes](#what-goes). The deletion may not proceed past them until
that transfer has happened, and "transfer" means the behaviour is under test in
anchor — not that the fix was described in a message.

Note that #107 is not reproducible locally on macOS — `cargo check --target
x86_64-pc-windows-msvc` dies in `libsqlite3-sys` for want of a Windows C
toolchain. The error list in that issue is CI's. Nobody should read a local green
as a refutation.

**Constraint 4 — every host running teleportd must have anchord first.**
Deleting the in-process layer makes `anchord` mandatory. Fleet audit,
2026-10-16:

| Host | teleport | anchord | `use_anchor` |
|---|---|---|---|
| `work_mac` | yes | yes | `true` |
| `mac` | yes | **no** | unset (`false`) |
| `main_pc` | yes | **no** | unset (`false`) |
| `mini_pc` | yes | **no** | unset (`false`) |

Three hosts run teleportd on the in-process path with no anchord present. The
delete strands them, and it strands them in the worst available shape: the
anchor client connects lazily, so teleportd would **start healthy and then fail
every session it was asked to create**. That is worse than refusing to boot,
because it looks fine until someone needs it — which is, by construction, the
moment they are away from the machine.

The fix is two lines of dotfiles per host (`components/anchord`, plus
`teleport_use_anchor: true`), the same pair `work_mac` already carries. But
applying it is a host change on each box and needs a per-box go-ahead. **Treat
this as a dependency with a human in it, not something that lands with the
merge.**

1. ~~**Resolve gap 12** with anchor.~~ Done — it was anchor's bug, fixed in
   `00f3eaa`. Teleport changes nothing.
2. ~~**Fix the remaining blockers.**~~ Done — gaps 1, 2 and 3, and #101. All
   were defects on the anchor path *today*, independent of any deletion.
   `session_events` (half of gap 2) is left open as non-blocking.
3. **Close the cheap divergences**, gaps 4 and 5, while both paths still exist
   and can be compared.
4. ~~**Package `anchord`**~~ Done. Running under its own lifecycle on
   `work_mac`.
5. ~~**Flip `use_anchor` to `true`.**~~ Done on `work_mac`, 2026-10-16. The flag
   stays in place, so the old path remains one config line away — and it is the
   rollback until step 7 removes it.
6. ~~**Run a release on the anchor path.**~~ Done at `4912d9c`; both exit
   criteria demonstrated. See [Status](#status--2026-10-16).
7. **Resolve Constraint 3** — anchor ships a Windows transport arm and teleport
   carries both arms behind one client type. Cross-repo; anchor leads. Three
   parts: the named pipe (mechanical), a DACL access-control story to replace
   the `0600` boundary (a scope change for anchor, which has deliberately had no
   auth), and ConPTY support — **the last of which is already solved and must be
   transferred from this repo, not rebuilt.**
8. **Satisfy Constraint 4** — anchord present on `mac`, `main_pc`, `mini_pc`,
   per-box go-ahead obtained. The dotfiles commit may be prepared ahead of this;
   it may not be applied ahead of it.
9. **Then delete**, in the order the seam collapses: config, `main.rs`,
   `api.rs`, `ws.rs`, then the modules. Keep the residue listed in
   [What stays](#what-stays).

Steps 2–3 are available now and are worth doing regardless of whether the
delete ever happens — they are bugs in the path teleport is moving to.

**The flag is the rollback, and step 9 is what spends it.** Up to that point
every step above is reversible by one config line. Deleting the in-process layer
is the first irreversible act in this sequence, which is why steps 7 and 8 are
preconditions rather than follow-ups: there is no second chance to discover that
a host or a platform was not ready.

## Minimum anchor version

Three fixes teleport now depends on, all found by auditing it against anchor's
protocol doc. Against an older anchord each is a live bug:

| Anchor commit | Why teleport needs it |
|---|---|
| `8b47537` | `log` clamps an oversized `max` instead of refusing it, so `get_log`'s paging loop works past 4 MiB. |
| `00f3eaa` | `terminate`'s reply means the exit is *recorded*, so the `purge` teleport sends next is not refused. |
| `77dde06` | `resize` travels outbound, so a third-party resize reaches teleport's browsers; the attach reply carries the starting size. |

Teleport's own half of that audit: input is split at 64 KiB (#97), so a paste
cannot drop an attach.

**Pending, and blocking step 9:** the Windows transport arm (Constraint 3). When
it lands it becomes a fourth row here and raises teleport's minimum anchor
version again — this time on a platform rather than a behaviour. Teleport cannot
state the version until anchor cuts it.

## Open questions

- **Gap 11**: do `log_warn_bytes` / `log_max_bytes` get deleted, or
  re-documented as anchord's to apply?
- **Gap 7**: is `delivered_to` or the authoritative offset the right
  `final_offset`? The anchor path's answer may be the better one, in which
  case the in-process path was wrong and this is a fix, not a regression.
- Does the GC task (`main.rs:453`) have any role once anchord owns session
  lifetime, or does it go with `recover()`?
- **Constraint 3**: what shape does the transport seam take? Teleport passes
  `anchor_socket` as a filesystem path today (`main.rs:576`); a named pipe is
  addressed differently, so either the config field becomes transport-agnostic
  or each platform gets its own. Anchor's design decides; teleport conforms.
- **Constraint 3**: is anchor's wire framing already transport-agnostic, or does
  it assume stream semantics a named pipe will not give it unchanged?
- **Constraint 4**: should teleportd refuse to start when it cannot reach
  anchord, once anchord is mandatory? Today the client connects lazily, which is
  what turns a missing anchord into "healthy but useless" rather than a loud
  failure. Lazy connect is right while the in-process path exists and arguably
  wrong after it goes.
