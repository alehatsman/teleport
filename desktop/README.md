# Teleport desktop shell (M10)

Thin Tauri window around `teleportd` + the web app. Design:
[08-packaging.md](../docs/08-packaging.md). Implementation spec and known
gaps: [11-mvp-plan.md#m10](../docs/11-mvp-plan.md#m10--tauri-shell).

## Build and run locally (Linux)

```bash
# 1. Build the daemon and copy it in as the sidecar.
(cd ../daemon && cargo build --release --features embedded-web)
../scripts/copy-sidecar.sh

# 2. Run in dev, or build a real .AppImage/.deb.
npm install
npx tauri dev
# or
npx tauri build --bundles appimage,deb
```

`copy-sidecar.sh` takes an optional target triple for cross-building (CI
passes one explicitly; local dev defaults to the host triple).

## What's real vs. scaffolded

Compiles clean (`cargo check` / `clippy -D warnings` / `fmt --check` /
`cargo test`) and was actually run end-to-end on Linux (2026-09-05): health
probe → spawns `teleportd` detached → creates a real session → the GUI
process is `kill -9`'d → daemon and session both survive → relaunching
re-probes and reattaches without spawning a second daemon. That's the M10
gate.

Re-run on macOS on 2026-09-05 (real hardware, Darwin 24.6.0 arm64, against a
real `Teleport.app` from `tauri build`, not `tauri dev`): same sequence, and
the quit step was a `kill -9` of the GUI's whole process group rather than one
pid, so `process_group(0)`'s detachment was tested rather than assumed. Passed
with no new bugs; details in
[11-mvp-plan.md#m10](../docs/11-mvp-plan.md#m10--tauri-shell).

Re-run on Windows on 2026-09-06 (real hardware, Windows 11 Pro build 26200)
in two passes. First pass used the raw `cargo build --release`
`teleport-desktop.exe` + sidecar (node/npm weren't installed yet), and
passed: launch/session/quit/survive/reopen/shutdown sequence, quit step a
single-pid `taskkill /F` (no Windows analog of a whole-process-group kill;
that harder case is exactly what the job-breakaway item below already
covers). Then node/npm were installed, the web UI and the daemon's
`embedded-web` feature were built for real, and the same sequence was re-run
against a real `.exe` installer from `npx tauri build --bundles nsis`,
silently installed with `/S` -- not `cargo build` output, an actual install.
That run caught a real bug the first pass couldn't have: the installed app
failed to launch at all (`STATUS_DLL_NOT_FOUND`) because `WebView2Loader.dll`
-- present next to `cargo build`'s own output, required by this project's
GNU/MinGW Windows toolchain (`rustup show`: `stable-x86_64-pc-windows-gnu`
is the active default here, not MSVC) -- was never in the installer's file
list, so the exe couldn't load, install and CI build-checks notwithstanding.
Fixed at the time with a new `tauri.windows.conf.json` declaring that dll as a
bundle resource (Tauri merges `tauri.<platform>.conf.json` automatically, so
this looked Windows-only and safe). Rebuilt, reinstalled, full gate re-run
end to end against the fixed installer with no manual workaround: passed,
including the real web UI actually rendering for the first time
(`embedded-web` couldn't be built at all before npm existed on this machine).

**Correction, 2026-09-06 (caught by another session's CI, not by this one):**
that fix broke CI. `tauri.windows.conf.json`'s resource path was right for
*this* machine's setup (GNU/MinGW toolchain, no `--target` flag, so
`WebView2Loader.dll` lands in plain `target/release/`) but CI builds
`x86_64-pc-windows-msvc` with an explicit `--target`
(`.github/workflows/desktop.yml`), which puts artifacts under
`target/x86_64-pc-windows-msvc/release/` instead -- and MSVC statically
links `WebView2Loader.dll` in the first place, so it never emits a loose
copy there at all. `tauri-build`'s own build script validates every
declared resource exists at *compile* time, so CI failed before bundling
even started, on every PR, Windows-unrelated or not. Removed
`tauri.windows.conf.json` entirely rather than trying to make one static
path satisfy two toolchains: MSVC (what CI and any real signed release
uses) needs nothing here since the loader is statically linked; a real
NSIS/MSI installer built locally with this repo's GNU/MinGW toolchain
still needs the manual copy this section already gives above --
`WebView2Loader.dll` next to `teleport-desktop.exe` in `target/release/`
-- before running `npx tauri build`, same as `copy-sidecar.sh` already does
for the daemon sidecar. The M10 gate pass recorded above genuinely happened
with that config in place and a real installed app; removing the config
now means reproducing that exact run again would need the manual copy step
back first -- not re-verified after this revert, since the point here was
restoring CI, not re-running the gate a third time.
Also caught in passing: the installer's default per-user install directory,
`%LOCALAPPDATA%\Teleport`, is the same physical folder as the daemon's own
data directory, `%LOCALAPPDATA%\teleport` -- NTFS is case-insensitive.
Confirmed harmless for now (the generated uninstaller deletes three named
files then a non-recursive `RMDir`, so it no-ops rather than touching real
data; the "delete app data" checkbox targets a different, unused
`$LOCALAPPDATA%\<reverse-dns id>` path instead), but an upgrade install while
`teleportd.exe` is still running detached in that same folder would collide
with a live-open executable -- not exercised here, tracked below rather than
fixed blind. Details in
[11-mvp-plan.md#m10](../docs/11-mvp-plan.md#m10--tauri-shell). So the gate is
now met on Linux, macOS, and Windows -- Windows against a real installer, not
just a compiled binary.

CI: `.github/workflows/desktop.yml` builds `desktop/src-tauri` (unsigned) on
macOS (both arches) and Windows on every push/PR -- build coverage only, not
part of the release pipeline (`release.yml` stays tag-only). Linux is still
only exercised by hand, per above.

Known gaps, tracked in [11-mvp-plan.md#m10](../docs/11-mvp-plan.md#m10--tauri-shell)
rather than silently papered over:

- **Windows detached-spawn** (`daemon::spawn_detached`) -- spiked for real on
  real native Windows hardware, 2026-09-05
  (`spike/src/bin/s11_windows_job_breakaway.rs`). Found a real bug along the
  way: `CREATE_BREAKAWAY_FROM_JOB` doesn't silently no-op when the
  *containing* job disallows breakaway, it fails the whole spawn with
  `ERROR_ACCESS_DENIED` -- exactly the launch contexts (an IDE debugger, a
  CI runner, some installer/sandboxing contexts) this flag exists to defend
  against. Fixed with a retry-without-the-flag fallback
  (`daemon.rs::spawn_windows_with_breakaway_retry`); the no-containing-job
  case (a plain double-clicked launch, the common one) and the
  containing-job-permits-it case both already worked and are unaffected.
  **Closed, 2026-09-06**: `daemon.rs`'s `job_breakaway_tests` module calls
  the real, unmodified `spawn_detached`/`spawn_windows_with_breakaway_retry`
  (not spike s11's reimplemented flags) against a real `teleportd.exe`, with
  the test process itself assigned to a real restrictive Job Object
  (`KILL_ON_JOB_CLOSE`, no `BREAKAWAY_OK`) -- the same shape a restrictive
  IDE debugger's or CI runner's containing job has. Run for real on this
  machine, twice (the restrictive-job case and the no-job baseline), both
  green. Surfaced one real gap while wiring this up: `spawn_detached` took a
  `data_dir` parameter but never forwarded it to the child as `--data-dir`,
  silently relying on the daemon's own default resolution landing on the
  identical path -- harmless in production (both sides compute the same
  default) but made isolated testing impossible without spawning a real
  daemon on top of the developer's actual data directory. Fixed by passing
  `--data-dir` explicitly; production behavior is unchanged. **Also closed,
  2026-09-06**: run again packaged inside a real NSIS installer (see the M10
  gate re-run above), not just a self-assigned job equivalent -- found a
  real missing-`WebView2Loader.dll` bundling bug in the process (see that
  section for the fix and the CI-breaking correction that followed it).
  Still open: launched by an actual IDE/CI job rather than a self-assigned
  equivalent -- lower priority now that both the function and the real
  installer are proven, since packaging doesn't change what `CreateProcessW`
  does with these flags (see s11's own doc comment).
- **Windows install-dir/data-dir collision**: the NSIS installer's default
  per-user install path and the daemon's own data directory are the same
  folder on disk (`%LOCALAPPDATA%\Teleport` vs `%LOCALAPPDATA%\teleport` --
  NTFS is case-insensitive). Confirmed harmless today (see the gate re-run
  above), but untested: installing an update while a previous `teleportd.exe`
  is still running detached from that same folder, since Windows generally
  won't let an installer overwrite a running executable. Not fixed --
  `bundle.windows.nsis.installMode`/a distinct install dir, or a
  stop-daemon-before-upgrade hook, are the candidate fixes, but this needs a
  real upgrade-over-a-running-daemon repro first, not a guessed fix.
- **Windows autostart** (`src/autostart/windows.rs`) is templated but
  unverified on real hardware.
  **macOS autostart closed, 2026-09-06**: `autostart/macos.rs`'s
  `launchagent_tests` drives the real `install()`/`uninstall()` against a real
  `~/Library/LaunchAgents`, `launchctl` and `teleportd`. `RunAtLoad` brings a
  serving daemon up, `KeepAlive` relaunches it after a crash, and a graceful
  SIGTERM is *not* undone by the agent -- so the tray's "Stop daemon" is not
  silently defeated by having autostart on. `#[ignore]`d (needs a real GUI
  login session): `cargo test -- --ignored --test-threads=1`. One gap found
  and documented rather than fixed: `KeepAlive { Crashed: true }` does not
  treat `SIGKILL` as a crash, so a force-quit or OOM kill leaves the daemon
  down until next login -- details in
  [11-mvp-plan.md#m10](../docs/11-mvp-plan.md#m10--tauri-shell).
- **Tray "Stop daemon" confirmation** (validation item 2) has only been run on
  Linux; it needs a human click, so the macOS pass above could not cover it.
  The endpoint underneath it (`POST /api/v1/shutdown`) *was* driven live on
  macOS.
- **Updater** is wired in (`tauri-plugin-updater`) but inactive
  (`tauri.conf.json`'s `plugins.updater.active: false`) -- no signed update
  artifact exists yet (signing is an external prerequisite, not a repo gap).
- **Signing/notarization**: not set up. Certs aren't in hand (confirmed with
  the user before scaffolding this); unsigned Linux builds work today,
  mac/Windows builds are unsigned until certs land.
