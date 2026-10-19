# 23 — Showing sessions teleport did not create

teleport lists only the sessions it created. On a machine where another anchor
client is also spawning agents, that makes the phone blind to most of what is
running. This doc specifies the one config knob that changes it, and — more of
the work — argues which of the four places that filter by owner should *not*
change.

Read [20-retiring-the-in-process-pty.md](20-retiring-the-in-process-pty.md) for
the anchor path this builds on, and anchor's `docs/01-scope.md` for why
visibility is a client concern in the first place.

## The workflow this is for

Agents are spawned at a desk from a hardware grid
([tally](https://github.com/alehatsman/tally): a Launchpad, one lit pad per
session, `anchor create --label tl.slot=…`). The same agents must be reachable
from a phone, over the tailnet, through teleport.

Both tools are anchor clients on the same host, against the same `anchord`.
Nothing needs to be integrated for this to work — that is the whole point of
anchor's model. One filter is in the way.

## The measurement

On `work_mac`, 2026-10-19, `use_anchor = true`, one `anchord`:

```text
$ tally ls           →  12 live sessions, 8 of them asking for attention
$ teleport sessions  →   2 sessions, both dead shells
```

The twelve are real, running, and attachable. teleport cannot see them because
`anchor_bridge::owned_by` asks `anchord` for `teleport.owner=teleport`, and
those sessions carry `tl.slot` instead.

## What already works, and is the reason this is small

**Every per-session handler on the anchor path already resolves by raw id, with
no ownership check.** This was not built for this feature; it is what the anchor
path has always done, because `anchord` indexes by id and an owner label is a
teleport concept.

| handler | resolves via | gated by owner? |
|---|---|---|
| `api.rs`'s `get_session` | `anchor.get(id)` | no |
| `api.rs`'s `delete_session` | `anchor.terminate` / `anchor.purge` | no |
| `api.rs`'s `get_log` | `anchor.get` + `anchor.log` | no |
| `api.rs`'s `resolve_ticket_session` | `anchor.get(id)` | no |
| `ws_anchor.rs`'s attach | `anchor.get(id)` | no |

A phone that knows a foreign session's ULID can already open it, stream it,
read its log and kill it, today, with no change to this repository. **Only
discovery is gated.**

`anchor_bridge::BridgedSession::kind` already says so in its own doc comment:

> Defaults to `"shell"` for a session anchor holds that **teleport did not
> create** — another client's session is still visible, and a missing label is
> not an error.

The bridge was written for foreign sessions. The list filter never caught up.

## The four sites, and why two of them stay

`anchor_bridge::owned_by` has four callers. Treating them as one decision is the
mistake this section exists to prevent — two of them are about *display*, two
are about *teleport's own accounting*, and only the first pair should widen.

| site | question it answers | change |
|---|---|---|
| `api.rs`'s `list_sessions` | *what does the operator see?* | **yes** — this is the gate |
| `anchor_watch`'s `spawn_supervisor` | *which streams do we scan for `title`, `last_bell_ms`, `idle_since_ms`?* | **yes** — without it foreign rows render with every derived field `null` |
| `api.rs`'s `create_session` (the `max_sessions` precount) | *how many sessions has teleport spawned?* | **no** |
| `api.rs`'s `health` (`sessions_running`) | *what does teleport have to lose if it restarts now?* | **no** |

**`max_sessions` must stay owner-scoped.** It is a runaway-loop guard on
teleport's own spawning (`docs/06-security.md#process-spawning`). Counting a
desk full of tally pads against it would refuse teleport's next launch for
reasons the operator cannot see from inside teleport.

**`sessions_running` must stay owner-scoped.** The updater reads it together
with `sessions_survive_restart` to decide whether it may restart under load.
The honest answer to *what do I have to lose* is teleport's own sessions; on the
anchor path a foreign session is unaffected by a teleport restart, so counting
it would block an upgrade that costs nothing.

Both of those are the existing `anchor_owner` doc comment being right. What that
comment conflates is the subject of the next section.

## What `anchor_owner` means after this

Today the field's doc comment reads, in part:

> Another tool's sessions are visible to anyone who asks anchord directly, but
> they are not teleport's to show or to count against its own `max_sessions`.

**"To show" and "to count" are two different claims, and only the second
survives.** Counting is accounting: a cap on teleport's own spawning, a restart
risk assessment, both correctly scoped to what teleport created. Showing is a
question about a human — and when the operator reading the phone is the same
person who spawned the pad at the desk, scoping their view to the tool that
happened to create the row answers a question nobody asked.

The field keeps its name and its job. It stops being the visibility rule, and
the doc comment is amended to say which of the two it still decides.

## Interfaces

```toml
# <data_dir>/config.toml
anchor_scope = "all"     # "owned" (default) | "all"
```

```rust
/// Which of `anchord`'s sessions teleport displays.
///
/// Orthogonal to `anchor_owner`, which still decides what teleport counts
/// as its own — see docs/23.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnchorScope {
    /// Only sessions carrying `anchor_owner`. The default.
    Owned,
    /// Every session in `anchord`, whoever created it.
    All,
}
```

The filter becomes a method on the scope rather than a free function, so the
two cases are one match instead of a branch at each call site:

```rust
impl AnchorScope {
    /// The label filter to send to `anchord`'s `list`.
    ///
    /// `All` is an **empty** filter, which is anchor's own spelling of
    /// "everything" — not a wildcard and not a second code path. anchor's
    /// `Store::list_filtered`: *"An empty filter is every session, so this
    /// is the general form of `list`."*
    pub fn filter(self, owner: &str) -> BTreeMap<String, String> { … }
}
```

`anchor_bridge::owned_by` is kept and called by `Owned`, so the two sites that
must stay owner-scoped keep calling it directly and cannot be widened by
accident.

**A string enum, not a `show_foreign` bool.** The two values name the decision —
*whose sessions is this daemon for?* — where a bool would name a mechanism. It
also leaves room for a future `owners = [...]` without a second flag, though
nothing asks for that yet and it is not built.

**Default stays `owned`.** A daemon that ships to someone sharing an `anchord`
between two teleports must not start showing them each other's sessions because
they upgraded.

## Consequences to accept deliberately

**Control preemption reaches further.** anchor's lease: attach never preempts,
an explicit claim does. So the phone opens a foreign session read-only, and
taking control preempts whatever holds the lease — for a tally pad, the tmux
pane at the desk. That is the correct and existing semantic, applied to more
sessions. It is listed here because it is the one that can surprise mid-session,
and it is the first thing to verify by hand.

**Kill and purge reach further.** A swipe on the phone is `anchor terminate` on
a session another tool is drawing on a hardware grid. Already true for anyone
holding the session id; this makes it reachable by browsing. Accepted — it is
the same operator.

**`kind` renders `"shell"` for every foreign session**, because `teleport.kind`
is absent. The payload already carries `command` and `args` from anchor's spec,
so the UI shows `claude` without inventing a label or asking tally to write one.
Fix in `web/`, same pass, cosmetic.

**No new privilege.** An authenticated teleport client can already spawn a
session with any `cwd` the daemon can read, and any OS user who can read
`<data_dir>/token` can reach `anchord`'s socket directly under the same uid.
Widening the list does not cross a boundary that was holding
(`docs/06-security.md#loopback-is-not-a-user-boundary`). **This argument expires
if teleport ever resolves more than one `Principal` to different authorities**
([12](12-identity-and-connectivity.md)); at that point visibility becomes a
policy question per principal, and this flag is the wrong shape for it.

**GC is not a hazard, and does not need a guard.** `run_gc_pass` iterates
`db.gc_candidates` — teleportd's own SQLite rows and directories under
`sessions_root`. A session teleport never created has no such row and cannot
become a candidate. The pass also never calls `anchor purge`, so even teleport's
own anchor sessions keep their anchord row. Nothing to change; stated here so
the question is not reopened.

## Why this is not fixed on the tally side

The one-line alternative is tally writing `teleport.owner=teleport` at spawn.
It is forbidden, in both repositories, deliberately:

> Label prefix. Ours, and **only** ours. teleport writes `teleport.*`; neither
> side ever writes the other's prefix, including "helpfully" copying one across.
> — tally `src/anchor.rs`, repeated in its `docs/01-scope.md`

It would also be false — teleport does not own those sessions — and would force
tally to forge `teleport.kind` as well to avoid every pad rendering as a shell.
That is one tool impersonating another to get past the other's filter.

**What a client displays is that client's policy.** anchor's scope doc puts
policy in clients, and this policy is teleport's. The change belongs here.

## Validation

- `config.rs`: `anchor_scope` absent parses to `Owned`; `"all"` parses to `All`;
  an unknown value is a parse error, not a silent default.
- `AnchorScope::filter`: `Owned` returns exactly `owned_by(owner)`; `All`
  returns an empty map.
- `api.rs`: with `All`, `list_sessions` returns a session carrying no
  `teleport.owner`; with `Owned`, it does not. One fixture, both assertions.
- `api.rs`: with `All`, a foreign session still does **not** count against
  `max_sessions`, and still does **not** appear in `health`'s
  `sessions_running`. These are the regressions this doc exists to prevent, so
  they get tests even though nothing changes at those sites.
- `anchor_watch`: with `All`, the supervisor registers a `Derived` for a foreign
  session, so `title` is non-null once that session has emitted one.
- The anchor-backed suite must run with `ANCHORD_BIN` set. Without it those
  tests early-return and print `ok` having executed nothing (#108, fixed for CI
  in `5db5fbd`) — the failure mode [20](20-retiring-the-in-process-pty.md)
  already warns about.
- Gate: `cargo clippy && cargo test` in `daemon/`, and
  `npm run lint && npm run typecheck && npm run build && npm test` in `web/` if
  the `kind` fallback lands in the same pass.
- **By hand, on `work_mac`:** set `anchor_scope = "all"`, spawn a pad from the
  Launchpad, confirm it appears on the phone over the tailnet with a title;
  attach read-only; take control and confirm the desk's tmux pane loses the
  lease and says so; detach and confirm the pad still works from the grid.

## Staging

| # | Commit | Touches |
|---|---|---|
| 1 | `feat(daemon): scope the anchor session list with anchor_scope` | `config.rs`, `anchor_bridge.rs`, `api.rs` (`list_sessions` only), `docs/23`, `docs/04` |
| 2 | `feat(daemon): watch every session in scope, not only our own` | `anchor_watch.rs`, `main.rs` |
| 3 | `fix(web): show the command when a session carries no preset` | `web/` |

1 and 2 are separable but only useful together — after 1 alone the phone lists
foreign sessions with no title and no attention badge. 3 is cosmetic and can
lag.

Provision carries the deployed value: the `anchor_scope` line belongs in
`components/teleport/templates/config.toml.j2` beside `use_anchor`, not
hand-edited into `<data_dir>/config.toml`.

## Not in scope

- **No per-owner session list on the API.** One flag, two values. A
  `?owner=` query parameter is a filter nobody has asked for.
- **No surfacing of foreign labels** through `/api/v1/sessions`. tally's
  `tl.attention` is a better attention signal than teleport's own
  `last_bell_ms`/`idle_since_ms` heuristics, and exposing it would let the phone
  show the same amber the grid does — but that is a second decision about
  whether teleport's API carries another tool's vocabulary, and it is not needed
  for the workflow above. Its own doc if it is ever wanted.
- **No write path for foreign labels.** teleport sets `teleport.*` and nothing
  else, for the same prefix-discipline reason tally does not set `teleport.*`.
- **No multi-host anything.** This doc is one `anchord` on one machine. Reaching
  a second machine's sessions is a transport question and is not teleport's
  (anchor `docs/01-scope.md`).
