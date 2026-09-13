// Regression test for this session's fix (Session.svelte, docs/09-frontend.md
// #control-lease-ui): a daemon restart must never leave a stale "Controlling"
// badge over a session that no longer has a PTY, and the header must say
// "Lost", not the generic "Closed" connection-state fallback.
//
// Owns its own dedicated daemon (not fixtures/index.ts's shared one) since
// killing a daemon mid-suite must never disturb whatever else is running in
// parallel against the shared instance.

import { expect, test } from "@playwright/test"
import { startDaemon, stopDaemon } from "./fixtures/daemon"

test("a killed and restarted daemon shows Lost, never a stale Controlling badge", async ({
  page,
}) => {
  let daemon = await startDaemon()
  try {
    const created = await fetch(`${daemon.baseURL}/api/v1/sessions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${daemon.token}` },
      body: JSON.stringify({ kind: "shell", preset: "shell", cwd: "/tmp", cols: 80, rows: 24 }),
    }).then((r) => r.json())

    await page.goto(`${daemon.baseURL}/?token=${daemon.token}#/sessions/${created.id}`)
    await page.getByRole("button", { name: "Take control" }).click()
    await expect(page.getByText("Controlling", { exact: true })).toBeVisible()
    await expect(page.getByText("Live", { exact: true })).toBeVisible()

    const { dataDir, port } = daemon
    await stopDaemon(daemon, { keepDataDir: true })

    // Same page, same tab, no reload -- exactly what a real client sees:
    // the connection drops out from under it while it's still mounted.
    await expect(page.getByText("Reconnecting", { exact: true })).toBeVisible()

    // Restart against the same data dir and the same port: the metadata
    // (SQLite + logs) survives a crash, only the live PTY registry doesn't
    // (docs/01-architecture.md#the-crash-boundary) -- and the already-open
    // page's socket can only ever reconnect to the port it was loaded from.
    daemon = await startDaemon({ dataDir, port })

    await expect(page.getByText("Lost", { exact: true })).toBeVisible({ timeout: 10_000 })
    await expect(page.getByText("Controlling", { exact: true })).not.toBeVisible()
    await expect(page.getByText("Closed", { exact: true })).not.toBeVisible()
    await expect(page.getByRole("button", { name: /Take control/ })).not.toBeVisible()
  } finally {
    await stopDaemon(daemon)
  }
})
