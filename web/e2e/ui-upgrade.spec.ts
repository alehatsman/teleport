// The claim docs/18-ui-upgrades.md makes, end to end: the web UI can be
// replaced under a *running* daemon, and the session that was live before
// the flip is still live and still attachable after it.
//
// Owns a dedicated daemon, started with no --web-dist so it resolves the
// <data_dir>/web/current slot the way an installed one does. Never touches
// the shared instance other specs run against.

import { cpSync, mkdtempSync, readFileSync, renameSync, symlinkSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { expect, test } from "@playwright/test"
import { type DaemonHandle, startDaemon, stopDaemon } from "./fixtures/daemon"

const DIST = join(import.meta.dirname, "..", "dist")

/**
 * Installs `npm run build`'s output as one version directory in the slot,
 * with a marker in the shell's <head> so a reload can be told apart from a
 * cache hit, and points `current` at it exactly the way `teleport ui
 * upgrade` does: symlink to a temp name, then rename(2) over `current`.
 */
function installVersion(dataDir: string, version: string): void {
  const slot = join(dataDir, "web")
  const dir = join(slot, version)
  cpSync(DIST, dir, { recursive: true })

  const index = join(dir, "index.html")
  const marked = readFileSync(index, "utf8").replace(
    "<head>",
    `<head><meta name="ui-version" content="${version}">`
  )
  writeFileSync(index, marked)

  const staged = join(slot, ".current.staged")
  symlinkSync(version, staged)
  renameSync(staged, join(slot, "current"))
}

async function uiVersion(daemon: DaemonHandle): Promise<string | null> {
  const res = await fetch(`${daemon.baseURL}/api/v1/health`, {
    headers: { authorization: `Bearer ${daemon.token}` },
  })
  const body = (await res.json()) as { ui_version?: string | null }
  return body.ui_version ?? null
}

test("the UI is replaced under a running daemon without losing the session", async ({ page }) => {
  const dataDir = mkdtempSync(join(tmpdir(), "teleport-e2e-slot-"))
  installVersion(dataDir, "v1.0.0")
  const daemon = await startDaemon({ dataDir, webDist: null })
  try {
    expect(await uiVersion(daemon)).toBe("v1.0.0")

    const created = await fetch(`${daemon.baseURL}/api/v1/sessions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${daemon.token}` },
      body: JSON.stringify({ kind: "shell", preset: "shell", cwd: "/tmp", cols: 80, rows: 24 }),
    }).then((r) => r.json())

    await page.goto(`${daemon.baseURL}/?token=${daemon.token}#/sessions/${created.id}`)
    await page.getByRole("button", { name: "Take control" }).click()
    await expect(page.getByText("Controlling", { exact: true })).toBeVisible()
    await expect(page.getByText("Live", { exact: true })).toBeVisible()
    await expect(page.locator('meta[name="ui-version"]')).toHaveAttribute("content", "v1.0.0")

    // The upgrade. One rename(2) -- nothing is signalled, the daemon is not
    // restarted, and the PID it started with is the PID it still has.
    const pidBefore = daemon.proc.pid
    installVersion(dataDir, "v1.1.0")
    expect(await uiVersion(daemon)).toBe("v1.1.0")
    expect(daemon.proc.pid).toBe(pidBefore)
    expect(daemon.proc.exitCode).toBeNull()

    // The socket never dropped: the page is still on the old bundle and
    // still attached, because a UI upgrade is not a daemon event.
    await expect(page.getByText("Live", { exact: true })).toBeVisible()

    await page.reload()

    await expect(page.locator('meta[name="ui-version"]')).toHaveAttribute("content", "v1.1.0")
    // The whole point: the same session, still running, still attachable
    // from the new bundle. Control comes back on its own -- the lease is
    // keyed to this client, and nothing about a UI swap released it.
    await expect(page.getByText("Live", { exact: true })).toBeVisible({ timeout: 15_000 })
    await expect(page.getByText("Controlling", { exact: true })).toBeVisible()

    // Attachable, not merely labelled as such: a real keystroke reaches the
    // PTY that predates the upgrade, and its output comes back.
    // (xterm.js exposes no accessible role for its canvas-rendered
    // terminal, so the hidden input-capture textarea is where a keystroke
    // lands -- same idiom as session-lifecycle.spec.ts.)
    const marker = `upgraded-${Date.now()}`
    await page.locator(".xterm-helper-textarea").click()
    await page.keyboard.type(`echo ${marker}`)
    await page.keyboard.press("Enter")
    // Two rows: the PTY's echo of the typed command, and its stdout below.
    await expect(page.getByText(marker)).toHaveCount(2)

    await page.screenshot({ path: "e2e/.tmp/ui-upgrade.png", fullPage: true })
  } finally {
    await stopDaemon(daemon)
  }
})

test("a stale tab's hashed assets still load from the retained version", async ({ page }) => {
  const dataDir = mkdtempSync(join(tmpdir(), "teleport-e2e-retained-"))
  installVersion(dataDir, "v1.0.0")
  const daemon = await startDaemon({ dataDir, webDist: null })
  try {
    await page.goto(`${daemon.baseURL}/?token=${daemon.token}`)
    // Whatever hashed chunk this build produced -- read off the page rather
    // than hardcoded, since the hash changes with every real build.
    const moduleScript = `script[type=${JSON.stringify("module")}]`
    const chunk = await page.locator(moduleScript).first().getAttribute("src")
    expect(chunk).toBeTruthy()

    installVersion(dataDir, "v1.1.0")

    // Exactly what a tab left open across the flip asks for on its next
    // lazy import: a chunk that now exists only in the retained directory.
    const stale = await fetch(`${daemon.baseURL}${chunk}`, {
      headers: { authorization: `Bearer ${daemon.token}` },
    })
    expect(stale.status).toBe(200)
    // Never the SPA shell: a tab that asked for JavaScript and got
    // text/html fails with a parse error that explains nothing.
    expect(stale.headers.get("content-type")).toContain("javascript")
    expect(stale.headers.get("cache-control")).toContain("immutable")

    const missing = await fetch(`${daemon.baseURL}/assets/index-deadbeef.js`)
    expect(missing.status).toBe(404)
    expect(missing.headers.get("content-type") ?? "").not.toContain("text/html")
  } finally {
    await stopDaemon(daemon)
  }
})
