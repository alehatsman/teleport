# 15 — Open questions and the M1 spike

Docs 01–14 describe a design. This file lists the places where that design is
**asserted but not yet proven**, and the decisions still genuinely open.

A doc stating a mechanism confidently is not evidence the mechanism works. Everything
below is a known gap, with the milestone it blocks and the thing that closes it.

**This file should shrink.** When an item is resolved, fold the answer into the doc it
belongs to and delete the entry here. An open-questions file that only grows is a
backlog pretending to be a spec.

## Status

| # | Question | Blocks | Closed by |
|---|---|---|---|
| [S1](#s1--who-reaps-the-child) | Who reaps the child, and what proves it exited? | M1 | spike |
| [S2](#s2--eof-is-not-exit) | Does EOF on the master mean the child exited? | M1 | spike |
| [S3](#s3--a-blocking-write-wedges-terminate) | Can a blocking PTY write wedge `terminate`? | M1 | spike |
| [S4](#s4--does-dropping-the-master-close-the-pseudoconsole) | Does dropping the master close the pseudoconsole on Windows? | M1 | spike |
| [D1](#d1--replay-must-not-share-the-live-subscriber-budget) | Replay shares the live subscriber budget | M4 | design change + test |
| [D2](#d2--session-list-freshness) | How does the session list stay fresh? | M5 | decision |
| [D3](#d3--attention-signals-in-the-mvp-ui) | Are `bell` / `idle` surfaced in the MVP? | M8 | decision |
| [N1](#n1--keystroke-latency) | Keystroke latency over a relayed tailnet | — | M9 measurement |
| [N2](#n2--websocket-compression) | WebSocket compression | M4 | half-day investigation |
| [N3](#n3--xtermjs-write-pacing-on-reattach) | xterm.js write pacing on reattach | M5 | decision |

S1–S4 and D1 are **blocking**. The rest are decisions that must be *made* before their
milestone, not necessarily *built*.

---

# The M1 spike

> **Do not write `pty.rs` until S1–S4 are answered by running code on Linux, macOS and
> Windows.**

`03-pty-layer.md` is correctly identified as the highest-risk file in the product. It is
also the file whose claims are least verifiable by reading. Budget one day. The output is
four answers and a decision on the per-session thread count — not production code.

The spike exists because [Thread model](03-pty-layer.md#thread-model) specifies **two**
threads per session while the design has **three** blocking concerns: `read()`, `write()`,
and waiting on the child. That arithmetic does not work, and every question below is a
consequence of it.

## S1 — Who reaps the child?

**Claimed:** [10-testing.md](10-testing.md#failure-injection-checklist) requires exit
codes to be recorded (`kill -9` an agent → "exit code recorded").

**Gap:** nothing in the thread model calls `wait()`. `portable_pty::Child::wait()`
blocks; `try_wait()` needs an owner and a cadence. Neither is specified. Today the exit
code has no path into the session row.

**Experiment**

- children that exit `0`, exit `7`, and are `SIGKILL`ed externally
- a child killed while producing output at full rate
- Windows: confirm the exit code is observable at all through a ConPTY child

**Pass criteria:** exit code observed within 100 ms of actual exit on all three
platforms, with no thread parked in `wait()` while a `write` or `terminate` is pending.

**Candidate answers — pick from the spike, do not pre-commit:**

| Option | Cost |
|---|---|
| Third dedicated thread per session, blocking `wait()` | one more thread per session; simplest and most obvious |
| `try_wait()` polled from the control thread on a 50–100 ms tick | no extra thread, but requires the control thread to never block — see [S3](#s3--a-blocking-write-wedges-terminate) |
| `SIGCHLD` handler | Unix-only; needs a Windows path regardless. Rejected unless the spike surprises us. |

## S2 — EOF is not exit

**Claimed:** the [Reader loop](03-pty-layer.md#reader-loop) breaks on `read() == 0`
("EOF: child exited or master closed") and the [State machine](03-pty-layer.md#state-machine)
reaches `EXITED` when the reader thread exits on EOF.

**Gap:** on Unix, EOF on the master means *every slave fd is closed* — not that the child
exited. The two diverge in both directions:

- a child that spawns a grandchild inheriting the PTY, then exits: the grandchild holds
  the slave open, **no EOF**, and the session sits in `running` with a dead child
- a child that closes its own descriptors and keeps running

This is the difference between "the agent finished" and "the session list is lying to
you" — and the session list is the product's front page.

**Experiment**

- `sh -c 'sleep 60 & exit 0'` — does `read()` return 0? when is the exit code available?
- Windows equivalent: a `cmd /c` that backgrounds a process and exits
- a child exiting mid-burst: confirm the output tail is fully drained before EOF is acted on

**Pass criteria:** `exited` and the exit code derive from the child wait, **never** from
EOF. EOF only stops the reader. The two events are handled correctly in either order, and
the state machine tolerates each arriving without the other.

This has a knock-on for the checklist: `10-testing.md` line "Kill an agent process
externally (`kill -9`) → reader sees EOF" encodes the assumption. Fix it when S2 closes.

## S3 — A blocking write wedges `terminate`

**Claimed:** [Thread model](03-pty-layer.md#thread-model) puts `write`, `resize` and
`terminate` on one control thread, on the grounds that commands "are short and
non-blocking except terminate, which is bounded."

**Gap:** writing to a PTY master **blocks** when the child is not reading stdin. The
kernel buffer is roughly 4–8 KiB on Linux. Meanwhile
[the fixture list](10-testing.md#1-pty-integration-fixtures-daemontestspty_rs) requires a
1 MiB write. While that write is blocked, `resize` and `terminate` cannot be serviced —
so `DELETE /api/v1/sessions/{id}` hangs and the bounded 5 s + 2 s wait in
[Concrete policy](03-pty-layer.md#concrete-policy) never even starts.

A session that cannot be killed because someone pasted into it is a shipping bug, and the
current thread model permits it.

**Experiment**

- a child that opens the tty and never reads it
- write 1 MiB; while blocked, issue `resize` and `terminate`
- measure time to `exited`

**Pass criteria:** terminate completes inside the documented bound regardless of a
pending write. Input that cannot be delivered is dropped or errored — never allowed to
hold the session open.

**Candidate answers:**

| Option | Cost |
|---|---|
| Separate writer thread; `terminate` stays on the control thread | one more thread; terminate is structurally never behind a write |
| Non-blocking master + bounded input queue, drop on overflow | gives input the same backpressure story as output; more code, and Unix/Windows diverge |

Either answer puts the per-session thread count at **three or four**, not two. That is the
honest price of the invariant. Pay it and update `03-pty-layer.md#thread-model`; do not
economize on threads and reintroduce the wedge.

## S4 — Does dropping the master close the pseudoconsole?

**Claimed:** [Termination](03-pty-layer.md#termination) step 2 gives the Windows graceful
stop as "`ClosePseudoConsole` via dropping the `portable-pty` master handle."

**Gap:** the reader thread holds a `try_clone_reader()` handle and the writer holds
`take_writer()`. Whether dropping the `MasterPty` closes the HPCON while those live is an
implementation detail of `portable-pty`, not a documented guarantee. If it does not,
graceful stop is a no-op on Windows and **every** close falls through to the hard kill —
which the platform checklist says loses the output tail.

Also unverified: the claim that `ClosePseudoConsole` returns immediately from build 26100
and can block on older builds. That claim is the entire reason the bounded wait sits on a
dedicated thread, and it is load-bearing for the M1 gate.

**Experiment**

- Windows 11 ≥ 24H2 **and** a build < 24H2
- close under heavy output load; measure how long the drop takes
- the grandchild case: a shell that started a background process — is the tree actually gone?

**Pass criteria:** a documented drop order that reliably closes the pseudoconsole on both
builds, with a measured upper bound on how long it can block. If dropping is unreliable,
call `ClosePseudoConsole` explicitly rather than relying on `Drop` ordering.

---

# D1 — Replay must not share the live subscriber budget

**Blocks M4. This is a design change, not a spike.**

Three facts that are individually right and jointly broken:

| Fact | Source |
|---|---|
| Subscriber queue bound: 256 chunks / 8 MiB | [03](03-pty-layer.md#backpressure) |
| `max_replay_bytes`: 8 MiB | [07](07-remote-access.md) |
| Attach order: register → capture `N` → replay `[requested, N)` → drain buffered ≥ `N` → live | [04](04-api-protocol.md#attach-race) |

The subscriber is registered and accumulating live output **for the entire duration of the
replay**. On a session emitting 1 MB/s — the load-sanity target in
[10](10-testing.md#load-sanity) — an 8 MiB replay to a phone on cellular takes tens of
seconds and buffers far more than the 8 MiB bound. The subscriber overflows and is
disconnected as a slow consumer *before it ever goes live*. It reconnects further behind
and fails again.

That is a livelock, and it triggers on precisely the session a user most wants to
attach to: a busy one. It will not show up in any test that attaches to an idle session.

**The attach ordering itself is correct and must not change** — it is what guarantees no
gap and no duplicate. What must change is the budget.

| Option | Notes |
|---|---|
| Spool live output to a separate bounded region during replay | preserves ordering; adds a second buffer to size and reason about |
| Replay to a moving boundary: read the file, re-take the mutex, repeat until the remaining gap fits the queue, *then* register | keeps exactly one buffer; several short mutex acquisitions; converges only while the client outruns the producer |
| Register with an unbounded queue, switch to bounded once live | simplest; reintroduces the unbounded memory the bound exists to prevent. Rejected. |

**Recommendation: option 2**, falling back to option 1 if convergence proves flaky under
a fast producer. Option 2 keeps one buffer in the design, which is the reason the current
backpressure story is easy to reason about.

Add to [10-testing.md](10-testing.md#2-sessionoffset-unit-tests) when this closes:

- attach with `after` far behind, against a session sustaining 1 MB/s → attach completes,
  no `1013`, no gap, no duplicate
- the same with `tail` unset, exercising `default_tail`

---

# Decisions to make

## D2 — Session-list freshness

**Blocks M5.** Unspecified anywhere: how the UI learns that a session exited, or that a
new one appeared, without opening its stream.

**Recommendation: poll `GET /api/v1/sessions` every 3 s while the list view is visible,
and stop on `visibilitychange`.** Boring, explicit, and correct. An events WebSocket is a
v2 addition and should not be built now.

The point of recording the decision is that it not be discovered halfway through M5 and
answered by whatever is quickest that afternoon.

## D3 — Attention signals in the MVP UI

**Blocks M8.** M2 records `bell` and `idle` rows in `session_events`
([05](05-persistence.md#schema)). **Nothing in the MVP ever reads them.**

Meanwhile [13-native-clients.md](13-native-clients.md#push-notifications) names *"an agent
is waiting for you"* as the feature that justifies the product existing, and
[08-packaging.md](08-packaging.md) lists OS notifications as in-scope for the Tauri shell
— which cannot be built, because attention state is not on the API.

**Cost to close:** two fields on the session-list payload (`last_bell_ms`,
`idle_since_ms`), a badge in the session list, a tray notification in M10. Roughly two
hours.

**Recommendation: take it in M8.** It is the highest ratio of user-visible payoff to
lines of code anywhere in the plan, and it converts the M10 notification bullet from
aspiration into something implementable.

**If rejected, delete the `bell` and `idle` event types from the schema.** Recording
events that nothing reads is dead weight that will rot into a lie about what the daemon
detects.

---

# Networking quality

None of these is a defect in what is written. They are the difference between "the
reconnect is correct" and "the reconnect is impressive", and the docs currently address
only the first.

## N1 — Keystroke latency

**Not blocking. Measure at M9.**

Every keystroke costs one full round trip before it appears: the daemon owns the PTY, so
a character is not on screen until the child has processed it and the bytes have come
back. This is inherent to the architecture and is the correct trade — but it sets a
ceiling on how the product *feels* that no amount of offset correctness can raise.

A phone on cellular reaching a workstation behind CGNAT frequently cannot establish a
direct WireGuard path and falls back to a Tailscale DERP relay. 80–200 ms is ordinary. At
that RTT a terminal feels broken.

Mosh exists largely to solve exactly this, via speculative local echo. That is a real v2
option, and it touches the protocol — the client must predict and then reconcile against
authoritative bytes. **Do not foreclose it.** Keeping replay a server-side decision (as
[04](04-api-protocol.md) already requires for snapshots) is what keeps it possible.

**MVP action: measure, do not build.** At the M9 gate record:

- RTT from the phone on cellular to the daemon
- whether `tailscale status` reports a direct connection or DERP for that peer
- subjective typing feel at that RTT

Write the numbers into this section. They decide whether predictive echo is a v2 item or
something the MVP cannot ship without.

## N2 — WebSocket compression

**Investigate before M4 closes. Half a day.**

Terminal output compresses roughly 10:1. `permessage-deflate` is negotiated at the
handshake, requires **no protocol change**, and would cut cellular bytes and shorten every
replay. If the pinned `axum` / `tokio-tungstenite` versions support it at acceptable cost,
it is the cheapest networking win available and it makes D1's replay problem smaller too.

**Do not compress above the WebSocket layer.** Compressing the payload ourselves would put
a codec between an offset and the bytes it indexes, which breaks the one invariant the
whole design rests on. Either the transport compresses, or nothing does.

## N3 — xterm.js write pacing on reattach

**Decide in M5.**

`default_tail` is 1 MiB ([07](07-remote-access.md)). Writing 1 MiB into `term.write()` in
one call stalls the render thread for seconds — at exactly the moment the product is
supposed to feel instantaneous. Bounded attach solved the *network* problem and left the
*rendering* one.

Two parts, both client-side:

- **the client chooses `tail`.** A phone should ask for ~256 KiB rather than inheriting a
  desktop default. The protocol already supports this; the web app just has to use it.
- **pace the replay write.** xterm.js exposes a `write(data, callback)` flow-control
  pattern; use it for replay so the first screen paints before the rest is fed in.

Add to the M5 gate: reattaching to a session with a large log paints the first screen in
under a second.

---

# What is deliberately *not* here

These were reviewed and found sound; they are recorded so they are not re-litigated.

- **The offset model.** Byte offsets as the only cursor, persist-before-advance,
  advance-before-fan-out, one mutex over `{next_offset, subscribers}`. Correct and
  internally consistent across docs 03, 04 and 05.
- **The control lease.** Attach-never-preempts plus a grace window is the right shape;
  it is the fix for a problem most implementations ship with.
- **The crash boundary.** Documented honestly rather than papered over, with the
  second-stage design sketched and correctly deferred.
- **Milestone ordering.** PTY first, UI last, Tauri after the product works. Right, and
  the discipline to keep it is worth more than any item above.

One caveat with no action attached: **the MVP's networking wow-factor is capped by
Tailscale onboarding.** "Install Tailscale on two devices, create an account, run
`tailscale serve --bg`, copy a token URL" is a competent fifteen-minute setup, not a magic
moment. The staging in [12](12-identity-and-connectivity.md) is right and the relay is
correctly deferred — but the magic lives in stage 3, and the MVP should be judged on
correctness, not on delight it cannot yet deliver.
