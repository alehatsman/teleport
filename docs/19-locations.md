# 18 — Locations: finding the folder you launch in

The launcher's working-directory field is the only thing standing between "I want an
agent here" and a running session. Today it is a text input plus a wall of full-path
chips ordered by recency alone. This doc specifies what replaces it.

Read [09-frontend.md](09-frontend.md) first for the launcher's component split, and
[04-api-protocol.md](04-api-protocol.md#get-apiv1browse) for the `browse` endpoint this
builds beside.

## Goal

Launching in a folder you use often takes **one tap, without reading a path**.

## What is wrong today

- `recentCwds()` (`web/src/features/sessions/sessionDisplay.ts:56`) dedupes session
  `cwd`s and sorts by **most recent use only**. A repo you open every day drops off the
  list the moment eight one-off directories are touched after it.
- The chips render the **full absolute path** in monospace
  (`SessionLauncher.svelte:195-207`), ellipsised at the container width. Eight of them
  are a block of near-identical `/Users/aleh/projects/…` prefixes — the distinguishing
  part is the part that gets clipped.
- There is **no search**. Past the eight chips the only route is `Browse…`, which walks
  the tree one tap per path segment.
- There is **no pinning**. A folder you return to weekly cannot be held in place, and
  the GC window (`retain_days`, default 14) eventually deletes the sessions the whole
  list is derived from.

## Model: a location

```ts
export type Location = {
  path: string        // absolute, as the daemon resolved it
  name: string        // basename -- what the user actually recognises
  parent: string      // dirname, home-collapsed to "~"
  uses: number        // sessions launched here, within the retained window
  lastUsedMs: number  // 0 for a pin that has no session history
  pinned: boolean
  score: number       // frecency, see below
}
```

`name` + `parent` exists because a path is read right-to-left but rendered
left-to-right: `teleport · ~/projects/futurumlab` is scannable, the full path is not.

### Frecency

One formula, no buckets, no tuning table:

```
score(location) = Σ over its sessions of 0.5 ** (ageDays / HALF_LIFE_DAYS)
HALF_LIFE_DAYS = 7
```

Each launch contributes 1 point that halves every week. Frequency is the sum; recency is
the decay. A folder used 10 times last week outranks one used once yesterday; a folder
untouched for a month sinks without being deleted. Pure function of
`(sessions, nowMs)` — `now` is passed in, never read inside (web/CLAUDE.md).

Ordering: **pinned first** (among themselves by score), then the rest by score, then by
`lastUsedMs` as a tiebreak so the order is total and stable across renders.

Input is the session list the launcher already has — no new endpoint for history, same
as today. The source is therefore bounded by `retain_days`; pins are what survives it.

## Stage 1 — ranked, labelled chips

No new component. A new module `features/sessions/locations.ts` owns the type, the
ranking and the matching — not `sessionDisplay.ts`, which is helpers over *one* `Session`
record, while this is a derived domain of its own with pins and queries in it.
`recentCwds()` is deleted; its caller and tests move over.
`SessionLauncher.svelte` renders each chip as `name` + dimmed `parent`, with the full
path in `title` for the fine-pointer case.

- Cap: **8 chips**. Every pin is shown first; frecency fills the remainder. More than 8
  pins means the tail is only reachable from stage 2's picker — acceptable, and the
  reason stage 2 is not optional.
- The `<datalist>` keeps the **full paths** — it is a typing aid, and a basename you
  cannot type is useless there.
- Prefill (`SessionLauncher.svelte:61`) switches from "most recent" to "highest ranked",
  which is the same thing on a fresh install and a better guess afterwards.

## Stage 2 — the location picker

New component `features/sessions/LocationPicker.svelte`, opened from the cwd field. It
is the full list; the chips are its top 8 hoisted inline.

```ts
{
  value: string                              // current cwd, for the "selected" mark
  locations: Location[]
  onSelect: (path: string) => void
  onTogglePin: (path: string, pinned: boolean) => void
  onClose: () => void
}
```

Layout, top to bottom: a search input (autofocused on fine pointers only — see
[Focus and the soft keyboard](#focus-and-the-soft-keyboard)), the filtered list, and
`Browse filesystem…` pinned at the bottom, which mounts the existing
`DirectoryBrowser.svelte` unchanged.

Matching is deliberately dumb and lives in `locations.ts` as
`matchLocations(locations, query)`:

- Query is lowercased and split on whitespace and `/`.
- A location matches when **every** term is a substring of its lowercased path.
- Results rank: matched-in-`name` above matched-only-in-`parent`, then by `score`.

No fuzzy-matching library, no scoring heuristics beyond that. `tel fut` finds
`~/projects/futurumlab/teleport`; that is the whole requirement.

Each row carries a pin toggle (a star), so pinning happens where the folder is, not in a
separate settings screen.

## Pins

Pins are **daemon-side**, so a folder pinned on the phone is pinned on the laptop.

**They do not live in `config.toml`.** That file is `Deserialize`-only, hand-authored,
and commented (`daemon/src/config.rs:39`); a write path from the API would rewrite a
user's file and mix app state into user configuration. Pins go in SQLite, which already
owns per-daemon mutable state and has the migration mechanism
([05-persistence.md](05-persistence.md#migrations)).

Migration 3:

```sql
CREATE TABLE IF NOT EXISTS pinned_locations (
  path         TEXT PRIMARY KEY,
  pinned_at_ms INTEGER NOT NULL
);
```

No foreign key to `sessions`: a pin outliving every session in that folder is the point.
GC never touches this table.

### `GET /api/v1/locations/pins`

```json
{ "pins": [{ "path": "/Users/aleh/projects/teleport", "pinned_at_ms": 1736790000000 }] }
```

Ordered by `pinned_at_ms` descending. A pinned directory that has since been deleted is
still returned — the client shows it, and launching there fails the same way any bad
`cwd` does. Silently dropping it would make a typo'd or temporarily-unmounted path
vanish with no way to unpin it.

### `POST /api/v1/locations/pins`

```json
{ "path": "/Users/aleh/projects/teleport" }
```

Canonicalised exactly as `browse` does (symlinks, `.`/`..`), so the stored path matches
the `cwd` a session records. `400` when it does not exist or is not a directory. Pinning
an already-pinned path is a no-op that returns the existing row — idempotent, because two
devices racing on the same star is normal.

Cap: **50 pins**, `429` past it with code `too_many_pins`. A list past 50 has stopped
being a shortlist.

### `DELETE /api/v1/locations/pins?path=…`

`204`, including when no such pin exists. Unpinning something already gone is success.

No new privilege in any of this: an authenticated client can already launch a session in
any path the daemon can read ([04](04-api-protocol.md#get-apiv1browse)).

### Client side

`api/api.ts` gains `listPins()` / `addPin(path)` / `removePin(path)`. `Sessions.svelte`
fetches pins beside the session list and owns the array. Toggling is **optimistic** —
the star flips immediately and reverts with an `ErrorBanner` on failure; a round trip to
the daemon before the star moves would feel broken over a tailnet.

## Focus and the soft keyboard

**iOS Safari zooms the page whenever a focused input renders below 16px.** Today every
form control does: `app.css:121` sets `input, select { font: inherit }` and
`.launcher__field` is `0.85rem` (≈13.6px), so opening the new-session dialog and tapping
any field zooms the viewport and leaves it zoomed.

Fix: inputs stop inheriting the label's size. In `app.css`, `input, select, textarea`
take `font-family: inherit` and `font-size: max(1rem, 16px)`, and the per-component
overrides that undercut it (`.session-filters__search`'s `0.9rem`) are removed. One rule,
no `@media (pointer: coarse)` fork, no per-component exceptions to keep in sync.

**`maximum-scale=1` / `user-scalable=no` is not an option** and never will be: it
disables pinch-zoom for everyone to paper over a font size. The viewport meta stays as it
is (`web/index.html:7`).

Related: the picker's search field is autofocused **only on fine pointers**. Auto-raising
the soft keyboard on open covers the list the user came to read.

## Staging

| # | Commit | Touches |
|---|---|---|
| 1 | `fix(web): stop iOS zooming when a form input is focused` | `app.css`, `SessionFilters.svelte` |
| 2 | `feat(web): rank launcher locations by frecency` | `locations.ts` (+tests), `sessionDisplay.ts`, `SessionLauncher.svelte` |
| 3 | `feat(daemon): pinned locations` | `persistence.rs`, `api.rs`, `04-api-protocol.md`, `05-persistence.md` |
| 4 | `feat(web): searchable location picker` | `LocationPicker.svelte`, `api.ts`, `Sessions.svelte`, `09-frontend.md` |

1 and 2 stand alone; 4 depends on 3 for the star.

## Validation

- `knownLocations` / `matchLocations`: unit tests in `locations.test.ts` — decay
  ordering (10 launches last week beat 1 yesterday), pins first, pins with no session
  history still listed, cap, tie-break stability, multi-term and `/`-split matching.
- Pins: `persistence.rs` tests (round trip, re-pin is idempotent, delete-missing is not
  an error, GC leaves the table alone) and an `api.rs` test for the non-directory `400`
  and the cap `429`.
- Gate: `npm run lint && npm run typecheck && npm run build && npm test` in `web/`,
  `cargo clippy`/`cargo test` for the daemon, per each CLAUDE.md.
- The zoom fix is **only** provable on a real device: `scripts/mobile-dev` with
  `safaridriver` ([09-frontend.md#mobile](09-frontend.md#mobile)). A desktop browser at
  phone width never zooms and will report success either way.
- Screenshots of the launcher at phone width before/after, per `web/CLAUDE.md`.

## Not in scope

- No per-device pin sets, no pin ordering by hand (frecency orders within pinned).
- No filesystem indexing or background scanning. The picker knows the folders teleport
  has launched in, plus what you pinned — it is not a file search tool.
- No git-repo detection or project-root inference. A location is a directory.
