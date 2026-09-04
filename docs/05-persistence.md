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
├── token              # 0600, only when auth_token is enabled
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

    created_at_ms   INTEGER NOT NULL,
    started_at_ms   INTEGER,
    exited_at_ms    INTEGER,
    exit_code       INTEGER,
    lost_reason     TEXT
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

`event_type` ∈ `created | started | resized | control_granted | control_revoked |
subscriber_attached | subscriber_detached | slow_consumer | terminate_requested |
exited | lost | bell | idle`.

`bell` and `idle` are **attention signals** — a BEL byte (`\x07`) seen in the output
stream, and output going quiet for N seconds while the process is still alive. The
reader loop already scans every byte, so detecting them is a few lines and costs
nothing. Recording them now means push notifications later are "deliver existing events"
rather than "add detection to the hot path"
([13-native-clients.md](13-native-clients.md#detection-heuristics)).

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

**Writes:**

- Buffered append; `write_all` per chunk.
- `fsync` on session close, and on a 2-second timer while running.
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
| `log_max_bytes` (default 1 GiB) | stop appending; set `log_capped = 1`; **keep streaming live output** |

Offsets stay monotonic forever. Replay works normally up to the cap point; beyond it,
the daemon reports a gap in `ready` rather than silently serving wrong bytes. Never
truncate from the head — that would invalidate every offset in flight.

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
        output_bytes = actual length of output.vt      ← file wins
```

**Trust the file's real length over a possibly-stale `output_bytes` column.** The
column is updated periodically and can lag a crash by seconds; the file cannot lie.

There is no PTY reattachment in MVP. See
[01-architecture.md](01-architecture.md#the-crash-boundary) for what would change that
and why it is deferred.
