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
gate, met on one platform.

Known gaps, tracked in [11-mvp-plan.md#m10](../docs/11-mvp-plan.md#m10--tauri-shell)
rather than silently papered over:

- **Windows "Stop daemon"** has no implementation yet -- no console-ctrl-event
  path exists for a console-less, autostart-launched `teleportd`. Needs
  either a small authenticated `POST /api/v1/shutdown` on the daemon or a
  console-ctrl-handler dance.
- **Windows detached-spawn** (`daemon::spawn_detached`) is written defensively
  (`CREATE_BREAKAWAY_FROM_JOB` et al.) but unverified on real Windows --
  spike this against a packaged build before trusting it.
- **macOS/Windows autostart** (`src/autostart/{macos,windows}.rs`) are
  templated but unverified on real hardware.
- **Updater** is wired in (`tauri-plugin-updater`) but inactive
  (`tauri.conf.json`'s `plugins.updater.active: false`) -- no signed update
  artifact exists yet (signing is an external prerequisite, not a repo gap).
- **Signing/notarization**: not set up. Certs aren't in hand (confirmed with
  the user before scaffolding this); unsigned Linux builds work today,
  mac/Windows builds are unsigned until certs land.
- **Icons** (`src-tauri/icons/`) are a placeholder solid-color "T" mark
  generated for this scaffold -- swap before any real release.
- **"NotOurs" / daemon-didn't-come-up** cases only log a warning today; the
  spec calls for surfacing them in-app (the dialog plugin is already wired
  in, just not used for this yet).
