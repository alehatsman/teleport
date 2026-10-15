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

Whole modules: `pty.rs` (except `login_shell_path`), `session/manager.rs`,
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
   teleport's `sessions` table on the anchor path. `title` and
   `claude_resume_id` live only in `WatchRegistry` memory
   (`anchor_watch.rs:68`) and are lost on every teleport restart — which
   contradicts the rationale at `api.rs:950-955`: "a resume id is only ever
   useful once the session that had it is gone, and the usual way it goes is
   the daemon restarting under it." The anchor path reintroduces exactly the
   bug that comment records fixing. `session_events` rows are never written
   either. **Blocker.**

   **Decided with anchor:** the two fields split on *write frequency*, not on
   ownership.

   - **`claude_resume_id` becomes an anchor label.** It is emitted once, in
     an agent's startup banner (`session/manager.rs:43-46`), so the write is
     rare. A label survives a teleport restart for free, needs no table, and
     creates no second source of truth. Anchor never interprets a label and
     has a test pinning that, so `teleport.claude_resume_id` stays
     self-evidently teleport's opinion.
   - **`title` stays in a teleport table.** It is re-emitted on every prompt
     (`session/manager.rs:349-353`) — chatty by construction, which is
     exactly why the in-process path throttles it to 1/s
     (`AGENT_META_PERSIST_INTERVAL_MS`, `session/manager.rs:41-47`). Labels
     have no rate protection and every label write is a SQLite write on
     anchor's side, so this one must not go there.

   That throttle is itself the evidence for the split: it exists because the
   title is chatty and the resume id is not.

3. **`controller` and `subscribers` are hardcoded** in list and get
   (`api.rs:908-909`) versus live values in-process (`api.rs:936-937`). The
   data is available — `leases.controller_name` is already used in the `ready`
   frame (`ws_anchor.rs:156`) — so the session list says nobody is in control
   while the WS says otherwise. **Blocker, cheap.**

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

   **Known limitation, confirmed with anchor.** The attach stream's `RESIZE`
   frame is client→daemon only; nothing fans geometry back out, so there is
   no way for teleport to learn about a third-party resize today. Do not add
   a poll for it. Surfacing it would be new protocol surface on anchor's
   side — proposed there, not assumed here. If anchor grows the frame, this
   gap closes by consuming it and this note goes.

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
`restart_recovery.rs`, `pty_primitive.rs`, `pty_primitive_windows.rs`,
`pty_term_default.rs`. Their *subject* is the deleted layer, not a behaviour
the product still has.

**Must be rewritten against the anchor path**, because they assert
product behaviour that survives: `ws_protocol.rs`, `http_api.rs`,
`reconnect_storm.rs`, `attention_signals.rs`, `osc_title_and_resume.rs`,
`pty_login_shell.rs`. Several have anchor counterparts already
(`anchor_ws.rs`, `anchor_http.rs`) — the rewrite is mostly checking which
assertions are missing there, not writing from scratch.

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
freed on purge (#101). Still missing: title and resume id surviving a teleport
restart (gap 2); `controller` in the session list matching the WS frame
(gap 3); close codes asserted by value (gap 5).

## Ordering

Nothing below moves until the one above it is true.

1. ~~**Resolve gap 12** with anchor.~~ Done — it was anchor's bug, fixed in
   `00f3eaa`. Teleport changes nothing.
2. **Fix the remaining blockers.** Gaps 2 and 3. ~~Gap 1 and #101~~ are done.
   Each is a defect on the anchor path *today*, independent of any deletion.
3. **Close the cheap divergences**, gaps 4 and 5, while both paths still exist
   and can be compared.
4. **Package `anchord`** as its own launchd/systemd unit with its own
   lifecycle (anchor M5). Until something starts anchord, flipping the default
   makes teleport depend on a process nothing launches.
5. **Flip `use_anchor` to `true`.** Ship it. Leave the flag in place so the
   old path is one config line away.
6. **Run a release on the anchor path.** This is the roadmap's gate and it
   cannot be shortcut: the in-process layer is the rollback.
7. **Then delete**, in the order the seam collapses: config, `main.rs`,
   `api.rs`, `ws.rs`, then the modules. Keep the residue listed in
   [What stays](#what-stays).

Steps 2–3 are available now and are worth doing regardless of whether the
delete ever happens — they are bugs in the path teleport is moving to.

## Minimum anchor version

Three fixes teleport now depends on, all found by auditing it against anchor's
protocol doc. Against an older anchord each is a live bug:

| Anchor commit | Why teleport needs it |
|---|---|
| `8b47537` | `log` clamps an oversized `max` instead of refusing it, so `get_log`'s paging loop works past 4 MiB. |
| `00f3eaa` | `terminate`'s reply means the exit is *recorded*, so the `purge` teleport sends next is not refused. |

Teleport's own half of that audit: input is split at 64 KiB (#97), so a paste
cannot drop an attach.

## Open questions

- **Gap 11**: do `log_warn_bytes` / `log_max_bytes` get deleted, or
  re-documented as anchord's to apply?
- **Gap 7**: is `delivered_to` or the authoritative offset the right
  `final_offset`? The anchor path's answer may be the better one, in which
  case the in-process path was wrong and this is a fix, not a regression.
- Does the GC task (`main.rs:453`) have any role once anchord owns session
  lifetime, or does it go with `recover()`?
