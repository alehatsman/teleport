# 08 — Packaging and desktop shell

The desktop package is a **delivery mechanism around the daemon + web app**. It is built
last, and it contains no session logic.

## What Tauri is allowed to do

```text
✓ open the UI (a WebView pointed at http://127.0.0.1:7337)
✓ install or locate the teleportd binary
✓ start teleportd if /health does not answer
✓ poll /health and show daemon status
✓ tray icon, menu, window management
✓ OS notifications (agent finished, agent needs input)
✓ application updates
✓ edit config.toml via the daemon or directly
```

## What Tauri must never do

```text
✗ own a PTY
✗ own a session
✗ define a Tauri-specific RPC protocol
✗ hold state the daemon does not have
✗ be required for a session to keep running
```

The desktop client speaks **the same HTTP + WebSocket API as the phone**. If a feature
needs a Tauri `#[command]` to work, it is in the wrong process.

## Daemon lifecycle — the important part

**Do not make the Tauri process the conceptual owner of `teleportd`.**

Bundling the daemon as a Tauri sidecar (`externalBin`, with target-triple-specific
artifacts) is a *distribution* convenience. Session survival must not depend on a
WebView window staying alive.

Startup logic in the shell:

```text
app launches
    ↓
read <data_dir>/port and <data_dir>/token
    ↓
GET http://127.0.0.1:<port>/api/v1/health
    Authorization: Bearer <token>    (when the token file was readable)
    ↓
├── 200, authenticated shape   → our daemon. Attach to it, open the UI.
│
├── 200, unauthenticated shape → something is listening but did not accept our
│                                token: another OS user's daemon, or our token
│                                file is stale. Do not attach — surface it.
│
└── refused / no port file    → start teleportd detached
                                 (not as a child that dies with the GUI)
                                 ↓
                              poll for the port file, then /health, up to 10 s
                                 ↓
                              ├── ok    → open the UI
                              └── fail  → show the daemon log, offer a retry
```

**The authenticated shape is the entire check.** Only a daemon holding our token can
return it, and only a process running as our OS user can read that token file — so the
shape itself proves ownership. Do not gate on `device_id`: it identifies the *host* to
remote clients ([12](12-identity-and-connectivity.md#the-principal)), and on a first run
the shell has nothing to compare it against.

On quit, the shell closes its window. **It does not stop the daemon.** Stopping the
daemon is an explicit user action in the tray menu, with a confirmation naming how many
sessions will be lost.

### Port discovery — do not hardcode 7337

`7337` is the default, not a guarantee. Two OS users on one machine both want it, and
so does whatever else happens to be listening. The daemon:

1. tries the configured port;
2. on `EADDRINUSE`, falls back to an ephemeral port (`127.0.0.1:0`);
3. writes the port it actually bound to `<data_dir>/port`, mode `0600`, and removes the
   file on clean shutdown.

Clients read that file rather than assuming. This matters beyond convenience: with a
hardcoded port, a shell whose own daemon failed to bind would happily health-check and
attach to **a different OS user's daemon**
([06-security.md](06-security.md#loopback-is-not-a-user-boundary)). The token check
stops it from doing damage; the port file stops it from trying.

### Updates must not kill sessions

The crash boundary ([01-architecture.md](01-architecture.md#the-crash-boundary)) is
usually discussed as though crashes were the only way to lose a session. They are not.
**Shipping an update is a daemon restart**, and it is the one that happens on a schedule
we control — the updater replaces the `teleportd` sidecar, the daemon restarts, and
every running agent dies.

Rules for the updater:

- Never restart the daemon while sessions are `running`. Stage the new binary and apply
  it on the next start when no session is live.
- If the user asks to update now, use the tray-quit confirmation: name the session count
  and make them confirm.
- Updating the **shell** alone is always safe — the WebView is disposable, which is the
  entire point of keeping the daemon out of the GUI process.
- A daemon that must restart marks its sessions `lost` / `daemon_restart` like any other
  restart. The UI states the truth; it never pretends a session survived.

### Autostart at login

The daemon should already be running before the GUI opens.

| OS | Mechanism |
|---|---|
| Linux | systemd **user** unit (`~/.config/systemd/user/teleportd.service`), `WantedBy=default.target`, **plus `loginctl enable-linger`** so it survives logout and a full reboot, not just re-login |
| macOS | `launchd` LaunchAgent in `~/Library/LaunchAgents/`, `RunAtLoad=true`, `KeepAlive` on crash |
| Windows | Task Scheduler task with a **logon trigger**, which exists specifically for starting an executable when a user logs in |

Autostart is user-scoped in every case. Never install a system-level service — that
would mean running as a different, more privileged user
([06-security.md](06-security.md#privilege)). Lingering does not cross that line: it is
a per-user systemd-logind flag, and enabling *your own* is unprivileged by default
(`org.freedesktop.login1.set-self-linger`'s polkit rule ships `allow_any=yes`).

**Headless install.** `autostart::install()` above is reachable only from the desktop
app's tray menu — no help on a box that has never run the GUI, exactly the machine
you'd reach from a phone over Tailscale. `teleportd service install` /
`teleportd service uninstall` does the same Linux install directly, no desktop app
needed (issue [#40](https://github.com/alehatsman/teleport/issues/40)). macOS and
Windows have no headless equivalent: a boot-time launch there needs a LaunchDaemon or
an elevated/unattended Task Scheduler trigger, both of which cross the privilege line
above, so those two stay login-scoped and reachable only from the tray.

## Build pipeline

```text
web/         →  vite build          →  web/dist/
daemon/      →  cargo build --release  →  teleportd[.exe]
desktop/     →  tauri build         →  .dmg / .msi / .AppImage / .deb
```

The daemon serves `web/dist` (via `ServeDir` in v1; consider embedding the assets in the
binary later so `teleportd` is a single self-contained file). SPA fallback: unknown
non-`/api` paths return `index.html`.

Tauri's `externalBin` picks up the target-triple-suffixed daemon binary
(`teleportd-x86_64-apple-darwin`, etc.). Build the daemon for each target before
`tauri build`.

## Signing

Signing is part of the release pipeline, **not** a last-minute step. Both Tauri and
Electron's distribution documentation treat platform signing as part of practical
macOS/Windows distribution, and unsigned applications hit increasingly restrictive OS
behavior.

| OS | Needed |
|---|---|
| macOS | Developer ID certificate, codesign, **notarization + stapling**; a hardened-runtime app that spawns a sidecar needs the right entitlements |
| Windows | Authenticode certificate; unsigned installers trip SmartScreen |
| Linux | No mandatory signing; sign the repo/AppImage if distributing one |

Set up signing infrastructure at the start of M10 (the Tauri milestone), not at
release. It routinely takes longer than the feature work it gates.

## Electron, if the stack ever changes

Recorded for completeness — not the plan. Electron is the credible alternative *when
using Node*: official packaging is Electron Forge. Its downsides here are a larger
privileged renderer surface (Chromium plus desktop APIs, requiring careful isolation,
sandboxing, no Node integration for remote content, restrictive CSP, validated IPC),
plus the same independent-daemon lifecycle problem *and* native `node-pty` packaging.
Choosing Electron does not remove the daemon; it just adds a runtime.

## Browser-only mode is a first-class deployment

Some users will never install the desktop app. `teleportd` + a browser is a complete,
supported product:

```bash
teleportd
# open http://127.0.0.1:7337
```

Every feature must work in that mode. If something only works inside Tauri, it is a
regression — the desktop shell adds polish, never capability.
