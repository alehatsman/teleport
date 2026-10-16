# 22 — The resume id, by injection rather than by scraping

Status: **spec, not implemented.** Tracking issue
[#69](https://github.com/alehatsman/teleport/issues/69).

## The problem

`claude_resume_id` is supposed to be the one-tap way back into the exact conversation a
dead session was having. It is read out of an OSC 8 hyperlink
(`https://claude.ai/code/session_<id>`) that Claude Code used to print in its startup
banner (`daemon/src/session/osc.rs:161`, `extract_claude_resume_id`).

**Claude Code no longer prints it.** Re-verified on this machine 2026-10-16 against
Claude Code **2.1.236** — the same build #69 was filed against, so the version has not
moved and nothing has been fixed upstream:

```
$ claude --version
2.1.236 (Claude Code)

# 20 live/closed teleport session logs under <data_dir>/sessions/*/output.vt
# count of OSC 8 sequences (ESC ] 8 ;) in any of them:
0
```

Four of those twenty logs do contain the literal string `claude.ai/code`, which is why a
naive `grep claude.ai/code` is misleading. Every one of those hits is plain rendered
text, not an escape sequence — the `/teleport` slash-command help line ("Control this
session from your phone or claude.ai/code") and this very source file being echoed into
a transcript. The scraper's input is gone. The OSC 0 title half still arrives and still
works.

So the degradation path documented in `04-api-protocol.md` and `05-persistence.md` is
working exactly as designed — null, never a parse error — but the field it protects now
carries nothing on every real session, and the scanner's OSC 8 half is dead code against
every Claude Code build teleport will meet.

## What already degrades correctly — leave it alone

Restore-by-respawn ([#67](https://github.com/alehatsman/teleport/issues/67),
[#65](https://github.com/alehatsman/teleport/issues/65)) does **not** silently do the
wrong thing with a null id. The rule is in one pure function:

- `web/src/features/sessions/launchRequest.ts:50-51` — with an id, `[...resume_args, id]`
  (`claude --resume <id>`); with no id but an explicit resume request,
  `[...resume_args]` (bare `claude --resume`), which opens Claude Code's own picker for
  that folder. Never `--continue`, which would point every agent restored out of one
  repo at one conversation.
- `web/src/features/sessions/SessionRow.svelte:211-215` — the Resume button is gated on
  the preset's `resume_args`, never on `claude_resume_id`, precisely because the id is
  null on every real session.

That is honest degradation, already shipped. This spec does not change it; it makes the
exact-conversation branch reachable again.

## The fix: ask for the id instead of reading it

Claude Code takes a caller-supplied conversation id. Verified by experiment on
2026-10-16, 2.1.236:

```
$ claude --session-id ce8883e3-eda1-4e89-a440-32a3b796f77b -p 'Remember the codeword BANANA7. Reply: stored'
stored
$ claude --resume ce8883e3-eda1-4e89-a440-32a3b796f77b -p 'what is the codeword?'
BANANA7
```

Three properties matter, all observed, none guessed:

1. **The id is honored.** The transcript lands at
   `~/.claude/projects/<cwd-slug>/<uuid>.jsonl` under exactly the uuid that was passed.
2. **The id is stable across a resume.** Every record in the resumed transcript still
   carries `"sessionId":"ce8883e3-…"`. A resume does not mint a new id, so teleport
   never has to re-learn one.
3. **Reuse is rejected loudly, not silently.** A second `--session-id` with the same
   uuid fails with `Error: Session ID <uuid> is already in use.` — which is the whole
   reason for rule R3 below.

This turns the resume id from something teleport scrapes out of a format it does not
control into something teleport *chose* before the process started. It cannot go stale,
because there is no upstream format left to break.

## Design

### Where the harness knowledge lives

In `presets.toml`, same seam as `resume_args` — the daemon must not learn that `claude`
takes `--session-id` and a plain shell does not (`daemon/src/presets.rs:35-46`).

New optional field on `Preset`:

```toml
[[presets]]
id = "claude"
resume_args     = ["--resume"]
session_id_args = ["--session-id"]   # new
```

`#[serde(default)]`, empty by default. Empty means "this agent cannot be told its own
conversation id" — no injection, today's behavior exactly. This is required, not
cosmetic: every already-installed daemon has a `presets.toml` on disk written before
this field existed, and `load_or_create` never rewrites an existing file
(`daemon/src/presets.rs:122-146`). Those installs keep working and keep getting a null
id until the user adds the line. Document that; do not add a migration that edits a
user's file.

### Rules

- **R1 — generate at create, not at first output.** `create_session` mints a fresh v4
  uuid (the `uuid` crate is already a dependency, `daemon/Cargo.toml:60` — no new deps)
  and appends `[...session_id_args, uuid]` to the resolved argv after `resolve_args`
  (`daemon/src/api.rs:727`, `daemon/src/api.rs:835`).
- **R2 — persist at create, not on a throttle.** The id is known before the PTY exists,
  so it is written with the session row, not flushed later by the OSC path
  (`daemon/src/persistence.rs:1375-1380`, `note_agent_meta`). On the anchor path it is a
  label set at creation (`teleport.claude_resume_id`,
  `daemon/src/anchor_watch.rs:51`) rather than one written by a watcher that saw a
  banner. Both paths get simpler, not more complex.
- **R3 — never inject onto a resume.** If the request's argv already carries the
  preset's `resume_args`, teleport appends nothing: the conversation keeps the id it
  already has (property 2 above), and injecting a second one would hit "already in use"
  and fail the spawn outright. The restored session's `claude_resume_id` is the id it
  was resumed from, copied forward.
- **R4 — never inject onto an explicit argv the caller wrote.** `POST /api/v1/sessions`
  with explicit `args` is the caller taking control of the command line; silently adding
  flags to it is the kind of magic this codebase rejects. Injection applies to the
  preset-default path only.
- **R5 — opaque on the wire.** `claude_resume_id` stays a `string | null` teleport never
  parses. Its shape changes from `session_<id>` to a bare uuid, and nothing in the API,
  the DB or the UI cares — `launchRequest.ts` only ever concatenates it.

### What this retires

Once R1–R5 land, `OscUpdate::ClaudeResumeId`, `extract_claude_resume_id`, and the OSC 8
arm of `OscScanner::finish` (`daemon/src/session/osc.rs:142-164`) have no live producer
and should be deleted along with the anchor-watch branch that consumes them
(`daemon/src/anchor_watch.rs:256-277`). The OSC 0/2 title half stays — it works, it is
the only thing in that scanner that does.

The `claude_resume_id` column stays. Dropping it would mean a migration to delete it and
a second migration to bring it back.

### A second reason to do this: it removes teleport's only live-session label write

`daemon/src/anchor_watch.rs:274` calls `anchor.label(&id, &set, &[])` on a session that is
already running, to attach `teleport.claude_resume_id` the moment the scanner sees one.
That is teleport's **only** label write outside `create`; everything else
(`labels_for`, `anchor_bridge.rs:32-39`) is set once at creation and never touched again.

It matters because of what a label write costs on the other side. anchor's `label` op
looked per-key on the wire but was a read-modify-write across two separate critical
sections ([anchor#12](https://github.com/alehatsman/anchor/issues/12)): two clients
writing disjoint keys to the same live session could lose each other's keys, last writer
replacing the whole map. anchor fixed it in `e1503c9` by holding one guard across read,
edit, write **and persist** — the persist being inside the guard is the non-obvious half,
since serializing only the in-memory edit still lets writers reach the store out of order
and lose the key again on restart.

So teleport is not exposed today, and will not be once that fix is deployed. But note
*why* it was not exposed before the fix: not because teleport only labels at create — it
does not — but because this particular call site has a dormant trigger, Claude Code
having stopped emitting the OSC 8 hyperlink that fires it
([#69](https://github.com/alehatsman/teleport/issues/69)). It is live code with a dead
producer, not a path designed never to run. Anyone re-enabling an OSC path, or adding any
other live-session label, would reintroduce a multi-writer story without noticing the
premise had moved.

Moving the id to a create-time flag deletes that call site, and teleport becomes
single-writer-at-create **by construction rather than by accident** — a property that
holds no matter what upstream does with its escape sequences.

### Validation

- `presets.rs`: an older `presets.toml` without `session_id_args` loads with the field
  empty and injects nothing (mirrors the existing `login_shell` back-compat test).
- `api.rs`: preset-default launch gets `--session-id <uuid>` appended; explicit `args`
  (R4) and a resume launch (R3) get nothing appended; a preset with empty
  `session_id_args` gets nothing appended.
- `persistence.rs`: the id is on the row the moment the session is created, and survives
  a daemon restart onto the recovered `lost` row.
- End to end, by hand against a real `claude`: launch from the preset, confirm
  `GET /api/v1/sessions` shows a non-null `claude_resume_id`, kill the daemon, Resume the
  `lost` row, confirm the conversation continues rather than the picker opening.

### Why this is a spec and not a patch

It changes the argv of every agent spawn. A wrong flag there does not degrade to null —
it turns every Claude launch into a failed spawn, for every user, on upgrade. That is a
judgment call with real blast radius across `presets.rs`, `api.rs`, `persistence.rs`,
`anchor_watch.rs` and the installed-`presets.toml` back-compat story, so it gets agreed
before it gets written.
