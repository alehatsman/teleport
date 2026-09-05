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
[11-mvp-plan.md#m10](../docs/11-mvp-plan.md#m10--tauri-shell). So the gate is
met on Linux and macOS; Windows is still build-only.

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
  Not yet run inside an actual packaged `.msi`/`.exe` installer or a real
  IDE/CI job (the spike simulates one directly via the same Win32 APIs) --
  that packaging-level pass is still open.
- **macOS/Windows autostart** (`src/autostart/{macos,windows}.rs`) are
  templated but unverified on real hardware. Unchanged by the macOS gate run
  above -- that exercised the launch/detach/reattach path, which never touches
  the LaunchAgent.
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
