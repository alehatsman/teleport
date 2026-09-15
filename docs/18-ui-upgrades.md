# 18 — UI upgrades without a restart

Read this when you touch how `web/dist` reaches a running daemon: `main.rs`'s
`--web-dist` resolution, `api.rs`'s `spa_fallback`, `embedded_web.rs`, the release
workflow's artifact list, or `teleport ui *`.

## Goal

> **Shipping a new web UI must not restart `teleportd`.**

A daemon restart kills every PTY it owns ([01-architecture.md](01-architecture.md#the-crash-boundary):
the update row). Today the UI is compiled *into* the binary, so changing one Svelte file
means a new `teleportd`, which means a restart, which means every running agent dies —
for a change that never touched a line of Rust
([08-packaging.md](08-packaging.md#updates-must-not-kill-sessions), issue
[#65](https://github.com/alehatsman/teleport/issues/65)).

Rust changes still need a restart. That is a separate problem with a separate answer
(in-place exec hot restart), explicitly out of scope here. This doc closes the half that
does not need any of that machinery, because the daemon already re-reads the UI from
disk on every request.

## What already holds

Three properties of the current code are what make this cheap. Do not break them:

1. **`ServeDir` opens the file per request** ([api.rs](../daemon/src/api.rs)'s
   `spa_fallback` constructs one per call). Files swapped underneath it are served
   immediately — no restart, no signal, no cache to invalidate in-process.
2. **A disk path always beats the embedded bundle**, deliberately, so a `--web-dist`
   pointed at a fresh `npm run build` overrides a baked-in bundle with no rebuild
   ([16-release-pipeline.md](16-release-pipeline.md#embedding-the-web-ui-embedded-web-feature)).
3. **The embedded bundle is the fallback**, so a binary with no assets beside it still
   serves a working UI. `curl … | sh` + `teleportd` stays one self-contained file, which
   is [16](16-release-pipeline.md#goal)'s whole promise.

This design adds a third resolution step between (2) and (3). It does not change either.

## The slot

```text
<data_dir>/web/
├── current -> v0.4.1          # symlink; the only thing a flip touches
├── v0.4.1/
│   ├── index.html
│   └── assets/index-<hash>.{js,css}
├── v0.4.0/                    # retained: stale tabs still request its hashed assets
└── v0.3.9/                    # retained
```

Resolution order for what `/` serves:

| # | Source | When |
|---|---|---|
| 1 | `--web-dist <path>` | Given explicitly **and** it resolves to a directory. The dev workflow; unchanged. |
| 2 | `<data_dir>/web/current` | It resolves to a directory. |
| 3 | Embedded bundle | Neither above resolves, in an `embedded-web` build. |
| 4 | `404` | Neither above resolves, in a plain build (`npm run dev` workflow). |

**`--web-dist`'s default changes from `web/dist` to unset.** Today it defaults to a
*cwd-relative* path ([main.rs](../daemon/src/main.rs)), which resolves during `cargo run`
from the repo root and never resolves under launchd/systemd — where cwd is `/`. A default
that only works in the one case the flag is passed explicitly anyway is not a default,
it's a trap. Unset means step 2 governs an installed daemon and step 1 governs a
developer, which is what each actually wants.

### Resolution is per request, not per process

`main.rs` decides the *path* once at startup. Whether that path currently resolves is
decided **per request**, in `spa_fallback`.

This is not a detail. `main.rs` today checks `is_dir()` once and stores
`Option<PathBuf>`, so a daemon that started before the first-ever `teleport ui upgrade`
would hold `None` and serve the embedded bundle until restarted — the exact restart this
doc exists to avoid, hit on the very first use. The slot path is therefore always
remembered, and the fallback to embedded is evaluated where the request is.

Cost: one `stat` per non-API request on a loopback HTTP server. Not a hot path, and the
`open` that follows would pay it anyway.

## Flipping

`teleport ui upgrade` — the **CLI** downloads, verifies and extracts; the daemon never
fetches anything (see [Why the CLI, not the daemon](#why-the-cli-not-the-daemon)).

```text
1. resolve release tag (latest, or --version vX.Y.Z)
2. if <tag>/ is already installed, skip to 7   ← re-extracting would mean deleting a
                                                  directory the daemon may be serving
3. download teleport-web-<tag>.tar.gz + checksums.txt
4. verify sha256 against checksums.txt        ← abort here leaves everything untouched
5. extract to <data_dir>/web/.staging-<tag>/  ← never over a retained dir
6. sanity-check the extracted tree            ← index.html exists and is non-empty
   then rename .staging-<tag> -> <tag>
7. symlink+rename <tag> over current          ← atomic; this is the upgrade
8. prune retained versions beyond the newest 3
```

Step 7 is `symlink()` to a temp name then `rename(2)` over `current` — atomic on POSIX,
so no request ever observes a missing or half-written `current`. The next HTTP request
serves the new bundle. **Nothing signals the daemon and nothing restarts.**

Steps 1–6 are all recoverable: any failure leaves `current` pointing where it did, and a
`.staging-*` directory to delete. Step 7 is the only irreversible one and it is a single
syscall. Step 8 runs after the flip, so a failure there has already left the upgrade
itself complete.

### Stale tabs, and why old versions are retained

A browser tab that loaded the old UI is still running old JavaScript. It will request
`/assets/index-<oldhash>.js` — a file that only exists in the previous version's
directory.

Serving that a `404` is bad. Serving it `index.html` is worse, and is what happens today:
`spa_fallback` answers *any* unmatched non-`/api` path with the SPA shell, so a stale
chunk request returns an HTML document with `Content-Type: text/html`, and the tab fails
with a parse error that says nothing about what happened.

Two rules:

- **A request under `/assets/` is never SPA-fallbacked.** It is a content-addressed file
  or it is a `404`. (True regardless of this design; fix it here.)
- **On a miss under `/assets/`, look in the retained sibling version directories** before
  giving up. Misses are rare by construction — only a tab that predates the last flip —
  so the directory scan sits on the miss path and costs nothing in steady state. No
  watcher, no in-process index, no invalidation.

Three retained versions is a guess (`RETAINED_VERSIONS` in `cli/src/ui.rs`). What it
buys: a tab open across two upgrades still works. What it costs: ~1.2 MB.

Retention and rollback order versions by directory **mtime**, i.e. install order, not by
name: version names don't sort (`v1.10.0` < `v1.9.0` lexically), and what both operations
actually mean is "the one before this one". The daemon's own miss-path scan orders by
name instead, because there it only needs to be deterministic — a hashed asset present in
two retained versions is the same bytes either way.

### Rollback

`teleport ui rollback` re-points `current` at the version installed immediately before
the current one. One `rename(2)`, same atomicity, no download. This is why a flip retains
the previous directory rather than replacing it in place.

It steps *backwards* specifically, rather than "to some other version": at the oldest
retained version it stops and says so. Picking "the newest one that isn't current" would
make a second rollback walk forward again, and the two commands would just toggle.

## Cache headers

**Mandatory, not polish.** Without them the flip is invisible for hours and the whole
design reads as broken.

| Path | Header | Why |
|---|---|---|
| `/index.html`, `/` | `Cache-Control: no-cache` | Must revalidate every load. This is the entrypoint that names the hashed assets; a cached one pins a tab to the old bundle indefinitely. `no-cache` means "revalidate", not "don't store" — the 304 is cheap. |
| `/assets/*` | `Cache-Control: public, max-age=31536000, immutable` | Content-hashed by Vite. A given URL's bytes never change, so this is free and correct. |
| everything else | unset | `ServeDir`'s `Last-Modified`/`If-Modified-Since` handling is fine. |

No cache-control layer exists today, which means browsers are applying *heuristic*
caching to `index.html` right now — a real, already-shipped bug for anyone who hard-
refreshes rarely.

## Telling the client

`GET /api/v1/health` already reports the daemon `version` and is already polled by the
app. It gains one authenticated field:

```json
"ui_version": "v0.4.1"
```

Read by `readlink`-ing the slot on each health request — one syscall, no parsing, no
manifest file to keep in sync. The value is **the directory name**, which is the release
tag by construction. `null` when serving the embedded bundle or an explicit
`--web-dist` (a dev tree has no version to report and must not pretend otherwise).

The web app compares it against the value it booted with and, on a change, offers
**"New UI available — Reload."** Not an automatic reload: a reload discards unsent
keystrokes and scroll position, and deciding that for someone mid-session is exactly the
kind of thing this product exists not to do. A reload is cheap and safe — the PTYs are in
the daemon, and the socket reconnects with replay from its last offset
([04-api-protocol.md](04-api-protocol.md#reconnect)) — but it is still the user's call.

The poll is on its own 30s timer, not the 3s session-list one, and the offer appears on
the list view only ([09-frontend.md](09-frontend.md#the-new-ui-available-offer)).

## Version skew is normal now

A UI upgraded independently of the daemon can be newer than the daemon it talks to. That
is not a new problem: `/health` already advertises `api_versions` and `capabilities`
precisely so a client can feature-detect, which any remote or native client needs anyway
([13-native-clients.md](13-native-clients.md)).

Rules:

- The UI targets the `v1` API surface and **feature-detects through `capabilities`**,
  never through a version comparison.
- `teleport ui upgrade` **warns** — does not refuse — when the bundle's tag is newer than
  the running daemon's `version`. Refusing would make the common case (UI-only fix
  shipped between daemon releases) impossible, which is the entire point.
- Nothing downgrades automatically. `teleport ui rollback` is the escape hatch.

## Release artifact

One new artifact, built once, target-independent:

| Artifact | Contents |
|---|---|
| `teleport-web-<tag>.tar.gz` | `web/dist`, with the release tag as the top-level directory name |

It is the *same* `npm run build` output the release already produces for the
`embedded-web` step ([16](16-release-pipeline.md#the-workflow)), so this adds a `tar` and
an upload, not a build. Its sha256 joins the existing `checksums.txt`.

The embedded bundle stays. A fresh install is still one self-contained binary; the slot
only exists after someone has upgraded the UI at least once.

### Extraction safety

The CLI writes into a directory the daemon serves, so the tarball is untrusted input
until verified:

- sha256 must match `checksums.txt` **before** anything is extracted.
- Reject any entry that is not a regular file, or whose resolved path escapes the staging
  directory (`..`, absolute paths, symlinks, hardlinks). No exceptions: nothing in a Vite
  build needs them.
- Create directories `0700`, files `0600`, owner-only, same as the rest of `<data_dir>`
  ([06-security.md](06-security.md)).

## Why the CLI, not the daemon

The daemon does not download anything. It resolves a path and serves files.

- **Attack surface.** `teleportd` is a long-lived process holding every PTY on the
  machine. Giving it an outbound update path — HTTP client, TLS trust decisions, archive
  extraction, checksum policy — puts all of that inside the process whose compromise is
  worst. The CLI is short-lived and holds nothing.
- **It already has the pieces.** `teleport` is a `reqwest` client with `--data-dir`
  resolution and token handling already written ([cli/src/](../cli/src/)).
- **It matches how upgrades already happen.** A provisioning run invokes commands; it
  does not ask a service to update itself. The dotfiles `components/teleport` step
  becomes "run `teleport ui upgrade`" instead of "bounce the service", which is the
  behavioral change this whole doc is for.

`teleport ui upgrade` writes to the **local** filesystem, so it is a local-daemon command
even though `teleport` is otherwise remote-capable. With `--url` pointing at a non-
loopback host it **fails with a clear error** rather than silently upgrading the wrong
machine's slot. Upgrading a remote daemon's UI means running the CLI on that host.

## Commands

```text
teleport ui status            # current version, retained versions, where it resolves from
teleport ui upgrade [--version vX.Y.Z]
teleport ui rollback
```

`teleport ui` is dispatched before the CLI resolves a daemon connection: it is
filesystem work on `<data_dir>/web` and must run whether or not a daemon is up. Failing
"upgrade the UI" because the daemon is stopped would be absurd.

`ui status` exists so the first question after a failed upgrade — "what is it actually
serving?" — has an answer that isn't `ls -l` on a symlink.

## What this does not solve

- **A Rust change still restarts the daemon and still kills every session.** In-place
  exec hot restart is the answer there; it is not this doc.
- **An open tab is not live-updated.** It is *offered* a reload. Hot-swapping a running
  SPA is not on the table.
- **This is not a rollback for the daemon binary.** Only the UI slot.

## Testing

Per [10-testing.md](10-testing.md)'s division: the daemon gets the serving behavior, the
CLI gets the file manipulation, and one e2e proves they meet. All of the below exist.

Daemon (`daemon/src/web_assets.rs` unit tests, `daemon/tests/web_static.rs`):
- A flip **while serving** — request, `rename(2)` a new `current` into place, request
  again, second response is the new bundle. No restart anywhere in the test.
- A daemon started with **no slot at all** picks up the first-ever `current` created
  under it without a restart (the `Option<PathBuf>`-at-startup regression).
- A request for a hashed asset that exists only in a **retained** version is served from
  there.
- A request for a hashed asset that exists **nowhere** is a `404` — never `index.html`,
  never `text/html`.
- `index.html` carries `no-cache`; `/assets/*` carries `immutable`.
- `/health`'s `ui_version` matches the slot, and is `null` under `--web-dist`.
- A **dangling** `current` (its target pruned) resolves to nothing, so the daemon falls
  back to the embedded bundle instead of serving 404s forever.

CLI (`cli/src/ui.rs` unit tests):
- A checksum mismatch aborts, and an archive absent from `checksums.txt` is an error.
- A tarball entry that escapes the staging directory is rejected, as is a symlink entry
  and an archive with no files at all.
- Extraction strips the tag directory and writes `0600` files into a `0700` directory.
- Retention keeps exactly the newest 3 behind the live one and never prunes `current`.
- `rollback` returns to the previous version and stops at the oldest retained.
- `--url` at a non-loopback host refuses.

E2E (`web/e2e/ui-upgrade.spec.ts`, against a daemon started with **no** `--web-dist` so
it resolves the slot the way an installed one does):
- With a session open, controlled and live, the slot is flipped, `/health` reports the
  new version, the daemon's pid is unchanged and its socket never dropped. After a
  reload the page is on the new bundle, the session is still live, still controlled, and
  a typed command still reaches the PTY that predates the upgrade.
- A stale tab's hashed chunk still loads after the flip, with `immutable` on it, and a
  chunk that exists nowhere is a `404` that is not HTML.

## Open questions

- **Retention count.** Three is still a guess, and deliberately still a constant rather
  than a config knob ([#79](https://github.com/alehatsman/teleport/issues/79)) — the real
  input is "how long does a tab stay open across upgrades", which nobody has measured.
  What changed is that the guess is now *falsifiable*: a hashed asset that misses the
  live slot **and** every retained version logs a `WARN` naming the path and the
  retention count (`spa_fallback`, `api.rs`). That line is the evidence that would move
  the number, and its absence is evidence three is enough. Adding the knob before the
  log had ever fired would have been tuning by imagination.
- **Who prunes.** `teleport ui upgrade` prunes old versions after a successful flip, and
  **sweeps every stray `.staging-*` before it starts** — not just the one its own tag
  would reuse ([#78](https://github.com/alehatsman/teleport/issues/78)). A retry after
  the crash that stranded one is exactly when a user is running `upgrade`, so that is
  where the sweep belongs. `teleport ui status` names any strays it finds, so the
  question "why is there a `.staging-v0.4.1` in my data dir" has an answer without
  reading this document. A daemon-side GC pass
  ([05](05-persistence.md#garbage-collection)) is still deferred: it would only matter
  for someone who strands a staging dir and then never upgrades again.
- **No release carries the artifact yet.** `teleport ui upgrade` cannot be exercised
  end to end against a real release until a tag is cut with the new
  `teleport-web-<tag>.tar.gz` in it. Everything up to and including the flip is covered
  by tests that build their own tarball; only the two `GET`s against github.com are not.
- **Desktop shell.** The Tauri build ([08](08-packaging.md)) bundles its own daemon
  sidecar; whether its tray offers "upgrade UI" separately, or the shell simply inherits
  whatever the slot holds, is not decided here.
