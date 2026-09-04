# 16 — Release pipeline (downloadable binaries)

Read this when you touch `.github/workflows/release.yml`, `scripts/install.sh`, or the
`embedded-web` cargo feature.

This is **not** the Tauri desktop package ([08](08-packaging.md)) — that's M10 and still
not built. This is the smaller, earlier thing: a way to get a working `teleportd` onto a
machine with one command, for the browser-only deployment [08](08-packaging.md#browser-only-mode-is-a-first-class-deployment)
already calls first-class. It ships as soon as CI is green on a tag; it does not wait for
M10 and does not touch the Tauri build.

## Goal

```bash
curl -fsSL https://raw.githubusercontent.com/alehatsman/teleport/main/scripts/install.sh | sh
teleportd
# open http://127.0.0.1:7337
```

One binary, no separate asset directory to keep alongside it, no signing step.

## Scope

In:
- Embed `web/dist` into the `teleportd` binary for release builds only, so the
  downloaded file is self-contained.
- A tag-triggered GitHub Actions workflow that builds `teleportd` for the target list
  below, packages each as an archive, and attaches them plus a checksums file to a
  GitHub Release.
- `scripts/install.sh`: detect OS/arch, fetch the matching archive for the latest (or a
  requested) release, verify its checksum, install the binary.

Out (tracked separately, not blocked on this):
- Tauri desktop packaging, signing, notarization, autostart units — all
  [08](08-packaging.md), all M10.
- A Windows-native installer or PowerShell one-liner. Windows users get a `.zip` release
  asset from the same workflow; a `curl | sh` script does not apply to `cmd`/PowerShell,
  and nothing here should imply it does.
- A Homebrew formula / Scapoop-style manifest, apt/rpm packages. Revisit once there's
  real install-count signal that raw binaries aren't enough.

## Targets

| Target triple | Archive | Notes |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `.tar.gz` | Matches CI's `ubuntu-latest` runner exactly — no new toolchain. Requires a glibc no older than the build image's; not chosen for maximum portability, chosen for zero new build complexity. |
| `x86_64-apple-darwin` | `.tar.gz` | Built on `macos-latest` (Apple Silicon runner) with the Intel target added via `rustup target add` + Xcode's cross-linker. |
| `aarch64-apple-darwin` | `.tar.gz` | Native on `macos-latest`. |
| `x86_64-pc-windows-msvc` | `.zip` | Built on `windows-latest`. |

Linux `aarch64` is deliberately not in the first pass — no arm64 GitHub-hosted runner in
this repo's plan yet, and cross-compiling `rusqlite`'s bundled SQLite adds real CI
complexity. Add it if someone asks for it.

## Embedding the web UI (`embedded-web` feature)

`daemon/src/main.rs` already resolves the SPA assets to serve at `/` from a `--web-dist`
path (default `web/dist`, relative to cwd), falling back to API-only when that path
isn't a directory ([08](08-packaging.md#build-pipeline)). That's right for local dev
(`npm run dev` never touches this path) and wrong for a binary someone drops in
`~/.local/bin` with nothing next to it.

Fix: a new, **optional** `embedded-web` cargo feature. Off by default — `cargo build`,
`cargo test`, `cargo clippy` in CI and local dev are unaffected and never need
`web/dist` to exist. On, only for release builds:

- `daemon/src/embedded_web.rs` uses `rust-embed` to bake `../web/dist` (relative to
  `daemon/Cargo.toml`) into the binary at compile time. The release workflow runs
  `npm run build` in `web/` *before* `cargo build --release --features embedded-web`, so
  the folder is populated when the macro reads it.
- `api.rs`'s `spa_fallback` keeps disk-backed `--web-dist` as the first check (so
  `--web-dist` still overrides an embedded build for local testing against a fresh `npm
  run build` without a rebuild). Only when `state.web_dist` is `None` **and** the feature
  is compiled in does it serve from the embedded bundle instead of falling through to
  `route_not_found`.
- Missing `web/dist` at compile time with the feature on is a **build failure**, not a
  silently-empty bundle — the release workflow must fail loudly rather than ship a
  `teleportd` with no UI.

This is exactly the embedding docs/08 already flagged as a good idea "later"
(08-packaging.md#build-pipeline) — done now because release packaging is what actually
needs it, not because M10 needs it yet.

## Versioning

- `Cargo.toml`'s `[package].version` is the single source of truth.
- A release is a tag `vX.Y.Z` matching that version exactly. The workflow's first job
  checks this and fails the run before building anything if they disagree — a version
  bump and its tag must never drift, and finding out from a confused user's install
  script is the wrong way to learn that.
- `teleportd --version` (via clap's `#[command(version)]`, already wired) reports the
  same string; `install.sh` doesn't need to parse it, but a support request can ask for
  it.

## Release workflow shape (`.github/workflows/release.yml`)

```text
on: push tags matching "v*"

job check-version (ubuntu):
    tag == "v${Cargo.toml version}" or fail

job build (matrix: the four targets above, needs: check-version):
    checkout
    setup rust (stable + target)
    setup node 22, npm ci, npm run build   (in web/)
    cargo build --release --features embedded-web --target <triple>  (in daemon/)
    package: teleport-<target>/teleportd[.exe] → .tar.gz / .zip
        (no LICENSE/README bundled -- neither exists at repo root today)
    upload-artifact (per target)

job publish (ubuntu, needs: build):
    download-artifact (all)
    sha256sum every archive → checksums.txt
    gh release create vX.Y.Z <archives> checksums.txt --generate-notes [--prerelease]
```

`gh release create` (already authenticated via `GITHUB_TOKEN`, default repo perms) is
enough — no extra release-automation action needed.

A tag with a semver prerelease suffix (`v1.2.3-rc1`, `v0.0.0-dev`) gets `--prerelease`
automatically — detected as a `-` after the leading `v`. Without it, `gh release
create` marks every tag "latest," and `install.sh` (plus the Releases page) resolves
"latest" to the newest non-prerelease release — a prerelease tag would otherwise
hijack that for real users pulling `curl | sh` with no `$TELEPORT_VERSION` set.

## `scripts/install.sh`

POSIX `sh`, no bashisms (`curl | sh` must work under whatever `/bin/sh` is). Behavior:

1. Detect `os` (`Linux` / `Darwin` — Windows is explicitly told to grab the `.zip` from
   the Releases page instead, not silently mis-detected) and `arch` (`uname -m`, mapped
   to the target-triple naming above).
2. Resolve a version: `$TELEPORT_VERSION` env var if set, else the GitHub API's
   `releases/latest`.
3. Download `teleport-<os>-<arch>.tar.gz` and `checksums.txt` for that release tag,
   verify the archive's checksum, abort on mismatch.
4. Extract `teleportd` to `${TELEPORT_INSTALL_DIR:-$HOME/.local/bin}` (created if
   missing), `chmod +x`.
5. Print the install path and, if that directory isn't on `$PATH`, say so explicitly
   rather than leaving the user to discover `command not found`.
6. Exit non-zero on any failed step — a partially-extracted binary must never look like
   a successful install.

No signature verification beyond the sha256 checksum in this pass (no code-signing
identity exists yet outside the M10 Tauri track — see [08](08-packaging.md#signing|); if
that changes, `install.sh` gains a step, not a rewrite).

## Edge cases

- **macOS Gatekeeper**: a binary extracted from a `curl`-fetched tarball doesn't carry
  the `com.apple.quarantine` xattr the way a browser download does, so it normally runs
  without a right-click-Open dance. If a user does hit a Gatekeeper block, that's a
  known rough edge of unsigned CLI distribution generally (rustup, many other CLI tools
  ship the same way) — not something `install.sh` can fix without the M10 signing
  pipeline. Document it in the release notes, don't silently paper over it.
- **`~/.local/bin` doesn't exist**: create it; don't fail because it's missing.
- **Re-running install.sh**: overwrite in place. Idempotent, no versioned side-by-side
  installs.
- **Tag pushed, version check fails**: workflow fails at `check-version`, before any
  build runs — cheap, fast failure, no half-published release.
- **`embedded-web` build with a stale or missing `web/dist`**: build fails (see above),
  caught in CI, never reaches a tag.

## Validation

- Push a `v0.0.0-test` tag (with a matching temporary `Cargo.toml` version on a scratch
  branch, never on `main`) to confirm the workflow end-to-end before the first real
  tag, then delete the test release/tag.
- After a real tag: run `install.sh` on a clean Linux and macOS machine (or container /
  VM — not a machine that already has `teleportd` built from source, to catch a `PATH`
  or overwrite assumption a dev machine would silently satisfy), confirm `teleportd`
  starts and `http://127.0.0.1:7337` serves the UI with no `--web-dist` flag.
