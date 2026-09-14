# 05 — Persistence

Two stores, because there are two data shapes: **relational metadata** and a
**high-rate append-only byte stream**. Do not put terminal bytes in SQLite rows.

## Layout

```text
<data_dir>/
├── state.db
├── state.db-wal
├── state.db-shm
├── config.toml
├── presets.toml
├── device.json        # stable device_id / device_name, generated on first run
├── token              # 0600, the local access credential — generated on first run
├── port               # 0600, the TCP port actually bound (see 08-packaging)
└── sessions/
    ├── 01K4N4ZP6C5GJ17G6X47K0VJX3/
    │   └── output.vt
    └── 01M2QF7T8H0BQV3W5Y61Z9AXKD/
        └── output.vt
```

`<data_dir>` resolution (via the `directories` crate):

| OS | Path |
|---|---|
| Linux | `$XDG_DATA_HOME/teleport` → `~/.local/share/teleport` |
| macOS | `~/Library/Application Support/teleport` |
| Windows | `%LOCALAPPDATA%\teleport` |

Overridable with `--data-dir`. Created with owner-only permissions
(`0700` on Unix) — see [06-security.md](06-security.md).

`device.json` holds a ULID generated once and never changed, plus a user-editable
display name defaulting to the hostname. Nothing in the MVP consumes it; it exists so
multi-device clients do not reshape every payload later
([12-identity-and-connectivity.md](12-identity-and-connectivity.md#device-identity)).

## Schema

```sql
CREATE TABLE sessions (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    preset          TEXT,
    command         TEXT NOT NULL,
    argv_json       TEXT NOT NULL,
    cwd             TEXT NOT NULL,

    state           TEXT NOT NULL,
    pid             INTEGER,

    cols            INTEGER NOT NULL,
    rows            INTEGER NOT NULL,

    output_bytes    INTEGER NOT NULL DEFAULT 0,
    log_capped_at   INTEGER,

    created_at_ms   INTEGER NOT NULL,
    started_at_ms   INTEGER,
    exited_at_ms    INTEGER,
    exit_code       INTEGER,
    lost_reason     TEXT,

    title             TEXT,
    claude_resume_id  TEXT
);

CREATE TABLE session_events (
    event_id        INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL,
    ts_ms           INTEGER NOT NULL,
    event_type      TEXT NOT NULL,
    data_json       TEXT,
    FOREIGN KEY(session_id) REFERENCES sessions(id)
);

CREATE INDEX idx_sessions_state   ON sessions(state);
CREATE INDEX idx_events_session   ON session_events(session_id, event_id);
```

`state` ∈ `running | closing | exited | lost`.

`lost_reason` ∈ `daemon_restart | spawn_failed | kill_timeout | io_error` (null
otherwise).

`io_error` is the one reason that can be set **while the child is still alive** — a
failed append does not kill the process. The session stays `running` with
`lost_reason='io_error'` and stops persisting output; it transitions to `exited` or
`lost` normally. Every other reason accompanies a terminal state.

`log_capped_at` is null until the log hits `log_max_bytes`, then holds the offset at
which persistence stopped. See [Size cap](#size-cap).

`event_type` ∈ `created | started | resized | control_granted | control_revoked |
subscriber_attached | subscriber_detached | slow_consumer | terminate_requested |
exited | lost | bell | idle`.

`bell` and `idle` are **attention signals** — a BEL byte (`\x07`) seen in the output
stream, and output going quiet for N seconds while the process is still alive. The
reader loop already scans every byte, so detecting them is a few lines and costs
nothing. Recording them now means push notifications later are "deliver existing events"
rather than "add detection to the hot path"
([13-native-clients.md](13-native-clients.md#detection-heuristics)).

### Agent-reported metadata

`title` and `claude_resume_id` are read out of the session's own output by
`session/osc.rs` — an xterm "set window title" sequence (OSC 0/2) and a Claude Code
resumable-conversation link (OSC 8). Neither is a teleport concept and neither is a
versioned contract; an agent that stops emitting them just leaves both null
([04-api-protocol.md](04-api-protocol.md#get-apiv1sessions)).

They are persisted, unlike `last_bell_ms`/`idle_since_ms`, because of **when** they are
useful. `claude_resume_id` exists to relaunch a conversation whose session can no longer
be attached to, and the most common way a session reaches that state is a daemon restart
— an upgrade or a reprovision ([01-architecture.md](01-architecture.md#the-crash-boundary):
the update row). Held in memory only, the id died in exactly the case it was for, and
every recovered `lost` session came back as an unlabeled row with no way back into its
conversation (issue
[#65](https://github.com/alehatsman/teleport/issues/65)). `title` rides along for the
same reason at list level: after a restart, "which of these eight is which" needs the
agent's own summary, and `command` + `cwd` alone do not carry it.

An attention signal answers "does this running session need you *now*" and is worthless
once the session is closed, which is why those stay live-only. These two are the
opposite: they matter most after the session is gone.

Write cadence, same discipline as `output_bytes`
([When `output_bytes` is written](#when-output_bytes-is-written)):

- Never on the per-chunk path. The reader loop records the new value in memory and marks
  the pair dirty; the write happens from the **existing throttled branch**, at most once
  per second per session, and only when something actually changed. A program that
  rewrites its title in a loop costs one write per second, not one per update.
- Once more when the session reaches `exited`/`lost` through this process, so a final
  title is not left up to a second stale.
- A write only ever *sets* a field it has a value for (`COALESCE(?, title)`); it never
  clears one. A session that emitted a resume id once and never again keeps it.
- A daemon killed between a change and the next flush loses up to one second of title
  churn. The resume id does not churn — Claude Code emits it in its banner, once — so in
  practice the field the restore path depends on is durable well before any restart.

Recovery needs no special case: the columns are ordinary session columns, they survive
the `running → lost` update untouched, and `GET /api/v1/sessions` serves them from the
row when there is no live session behind it. No index — nothing queries by either.

**`env` is deliberately absent from the schema.** Agent environments routinely contain
API keys. Reconnecting a terminal does not require storing them. Store `command`,
`argv_json`, `cwd` and redacted metadata only.

## Migrations

`PRAGMA user_version` as the migration counter. Migrations are an ordered `&[&str]`
array in `persistence.rs`; on open, apply every migration with an index `>=
user_version`, then set `user_version`. No migration framework, no external tool.

Pragmas set on every connection:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
```

WAL lets readers and one writer proceed concurrently. SQLite still allows only one
writer at a time — which is fine, because there is exactly one authoritative daemon.
WAL's automatic checkpointing handles an ordinary workstation workload; do not add
manual checkpoint logic without evidence.

## One writer, no pool

All database access goes through a **single writer actor** on a dedicated thread owning
one `rusqlite::Connection`, fed by a `tokio::sync::mpsc` command channel.

```text
async handlers ──mpsc──▶ DbActor thread ──▶ rusqlite::Connection
                 ◀──oneshot── result
```

Reads go through the same actor. The query volume is trivial (a session list, a session
row) and a single path is simpler than a pool plus a writer. Explicit beats magic.

If read latency ever becomes measurable, add read-only connections then — not now.

## Output log

`output.vt` is raw PTY bytes, appended, never rewritten. No framing, no headers, no
escaping. `file_length == next_offset`. That identity is the entire replay index.

There is exactly one exception — a capped log — and it is the reason `log_capped_at`
exists. The general form of the invariant, which is the one to assert in tests:

```text
file_length == min(next_offset, log_capped_at)      # log_capped_at = ∞ when null
```

Offsets are handed out by the reader loop and **never rewind**, capped or not. What a
cap changes is only whether the bytes behind an offset are still on disk.

**Writes:**

- Buffered append; `write_all` per chunk. **The buffer is the page cache, not a
  userspace `BufWriter`** — replay reads through a separate handle, and a byte still
  sitting in a userspace buffer is not visible to it, so buffering there would publish
  offsets whose bytes a reconnect cannot read.
- `fsync` on session close, and on a 2-second timer while running. **Neither runs on the
  reader thread or under the fan-out mutex** — an `fsync` held there stalls the PTY
  behind disk latency and blocks every concurrent attach. One `LogSyncer` thread for the
  whole daemon holds a second reference to each log's open file and flushes them on the
  timer; close-time sync goes through the same handle.
- Never `fsync` per chunk — it would couple PTY drain rate to disk latency and put the
  reader thread at risk of falling behind under load.

**Reads** (`/log` and WS replay):

- Open a separate read handle, `seek(from)`, stream to the client.
- Reading past `next_offset` is a bug in the caller; the daemon clamps to
  `next_offset` and never reads bytes it has not accounted for.

### Size cap

Unbounded terminal output is a real disk-exhaustion risk (a runaway build, a `yes`
loop). Policy:

| Threshold | Behavior |
|---|---|
| `log_warn_bytes` (default 256 MiB) | emit a `session_events` warning; UI shows a badge |
| `log_max_bytes` (default 1 GiB) | stop appending; set `log_capped_at = next_offset`; **keep streaming live output** |

A chunk that straddles `log_max_bytes` is written up to the limit and truncated there,
so `log_capped_at == log_max_bytes` exactly for any log capped by growing. A log whose
`next_offset` is ahead of its file for any *other* reason — an `io_error`, or a restart
whose stored `output_bytes` exceeds the file length — reports `log_capped_at` at the
file length, so "what is readable" has one answer and every read clamps to it.

This is why a failed append **sets `log_capped_at` too**, not just `lost_reason`. A hole
that is not reported as a cap is served as a replay range that silently stops short
while live output resumes past it — a gap the client is never told about, which is
exactly what the offset model exists to make impossible.

Never truncate from the head — that would invalidate every offset in flight.

Once `log_capped_at` is set, the file stops growing while `next_offset` keeps
advancing. Everything that reads the log must respect that:

```text
replay range          → clamped to [from, min(to, log_capped_at))
after >= capped_at    → no replay at all; ready reports replay_from = next_offset
/log                  → serves up to log_capped_at, then stops
ready                 → carries log_capped_at so the client can render the gap
```

The bytes between `log_capped_at` and `next_offset` were streamed live to whoever was
attached and are **gone**. That is a real hole in history, and the API says so rather
than serving whatever happens to be at that file position
([04-api-protocol.md](04-api-protocol.md#bounded-attach)).

### Garbage collection

On startup and every 6 hours: delete `sessions/<id>/` for sessions in state `exited`
or `lost` whose `exited_at_ms` is older than `retain_days` (default 14). Delete the
`sessions` row only after the directory is gone, so a crash mid-GC leaves a row whose
log is missing rather than a log with no row. `/log` on a GC'd session returns `410`.

## In-memory ring (optional)

A small per-session RAM ring (a few MiB) makes common short reconnects cheap by
avoiding a file seek + read. It is a **cache, not the durability model** — the file is
always authoritative for older replay.

**Ship the MVP without it.** Add it only if profiling shows reconnect file reads matter.
Correctness must not depend on it.

## Restart recovery

```text
open state.db, run migrations
    ↓
for each row where state IN ('running','closing'):
        state       = 'lost'
        lost_reason = 'daemon_restart'
        exited_at_ms = now
        append session_events(lost)
    ↓
for every session directory:
        output_bytes = max(len(output.vt), stored output_bytes)
```

**Take the larger of the two.** For a normal log the column lags a crash by seconds and
the file wins — the file cannot lie about bytes it holds. For a *capped* log the file
stopped growing while the column kept counting, so the column wins. Taking the file
length unconditionally would rewind `next_offset` below offsets already handed to live
clients, which turns every reconnecting client into an `offset_ahead` error and makes
the daemon hand out offsets it has already used.

### When `output_bytes` is written

The `max()` rule is only worth anything if the column is being maintained. Write
`output_bytes` — together with `cols` and `rows` — on a **throttled schedule, at most
once per second per session** while output is flowing, plus once on the transition to
`closing` / `exited`. Never on the per-chunk path: that would put SQLite in the PTY
drain loop and couple throughput to disk latency ([above](#output-log)).

A column written *only* at exit silently degrades `max()` to file-wins. A capped session
killed before it exited would then come back with `next_offset` below offsets live
clients already hold — precisely the rewind the column exists to prevent. The cadence is
the mitigation; the column alone is not.

A recovered session is `lost` and will never produce another byte, so the few seconds
of column lag on a capped session cost nothing.

There is no PTY reattachment in MVP. See
[01-architecture.md](01-architecture.md#the-crash-boundary) for what would change that
and why it is deferred.
