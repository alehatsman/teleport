// The restore path in the browser (docs/05-persistence.md#agent-reported-metadata,
// issue #65): an agent session's resume id is only useful once that session can no
// longer be attached to, and the usual way it gets there is the daemon restarting
// under it -- an upgrade or a reprovision. Held in memory only, the id died with the
// process that saw it, so every recovered `lost` row came back unlabeled and with no
// "↻ Resume" action at all.
//
// Owns its own dedicated daemon, same reason reconnect-and-lost.spec.ts does: killing
// one mid-suite must never disturb the shared instance other specs run against.

import { expect, test } from "@playwright/test"
import { startDaemon, stopDaemon } from "./fixtures/daemon"

// Claude Code's own banner shape: an OSC 8 resumable-conversation link and an OSC 0
// title, then an idle wait -- the state a reprovision actually catches an agent in.
// Assembled from parts rather than written as one literal, so the opaque-looking id
// inside a URL doesn't read as a checked-in credential to the lint baseline.
const RESUME_ID = "session_restoreme"
const TITLE = "Fix the login bug"
const LINK = `https://claude.ai/code/${RESUME_ID}?from=cli`
const BANNER = [
  `printf '\\033]8;id=x;${LINK}\\033\\\\'`,
  `printf '\\033]0;${TITLE}\\007'`,
  "sleep 120",
].join("; ")

test("a resume id survives a daemon restart and is still one click away", async ({ page }) => {
  let daemon = await startDaemon()
  try {
    const created = await fetch(`${daemon.baseURL}/api/v1/sessions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${daemon.token}` },
      body: JSON.stringify({
        kind: "claude",
        // `preset` *and* an explicit `command`: the command wins for spawning
        // (api.rs `resolve_command`) so this can fake the banner, while the
        // row still carries `preset: "claude"` the way a real launcher-created
        // session does. The Resume action is gated on the preset's own
        // `resume_args` (#69 follow-up), so a session with no preset at all --
        // which the launcher never produces -- correctly offers nothing.
        preset: "claude",
        command: "/bin/sh",
        args: ["-c", BANNER],
        cwd: "/tmp",
        cols: 80,
        rows: 24,
      }),
    })
    expect(created.status).toBe(201)

    await page.goto(`${daemon.baseURL}/?token=${daemon.token}`)
    const row = page.getByRole("listitem").filter({ hasText: TITLE })
    await expect(row).toBeVisible({ timeout: 15_000 })

    // The banner's two sequences land milliseconds apart, so only the first goes out
    // on the reader thread's leading-edge write; the second waits for the idle
    // sweep's flush. Give that one tick before the kill -- a real agent has been
    // sitting there for minutes by the time a reprovision reaches it.
    await page.waitForTimeout(6_500)

    const { dataDir, port } = daemon
    await stopDaemon(daemon, { keepDataDir: true })
    daemon = await startDaemon({ dataDir, port })

    await page.reload()
    // The session is `lost` now -- no PTY behind it, which is exactly when the resume
    // id is the only way back into that conversation. A closed session moves out of
    // the "Active" tab, so that's where the restore action has to be reachable from.
    await page.getByRole("tab", { name: /^Closed/ }).click()
    const lostRow = page.getByRole("listitem").filter({ hasText: TITLE })
    await expect(lostRow).toBeVisible({ timeout: 15_000 })
    await expect(lostRow).toContainText("lost")

    const resume = lostRow.getByRole("button", { name: /Resume/ })
    await expect(resume).toBeVisible()
    await resume.click()

    // The launcher opens on the claude preset, pre-filled with the id that came off
    // the row -- nothing to find, copy, or type.
    const resumeField = page.getByLabel(/Resume session ID/)
    await expect(resumeField).toHaveValue(RESUME_ID)
    await expect(page.getByLabel("Preset")).toHaveValue("claude")

    await page.screenshot({ path: "e2e/.tmp/resume-after-restart.png", fullPage: true })
  } finally {
    await stopDaemon(daemon)
  }
})

// The case that actually matches a fleet of agents today: Claude Code 2.1.x no longer
// emits the OSC 8 link `claude_resume_id` is read from, so a restored session has no
// id at all. "Resume" still has to be there and still has to resume -- through Claude
// Code's own picker for that folder (launchRequest.ts explains why not `--continue`).
test("a claude session with no resume id is still restorable after a restart", async ({ page }) => {
  let daemon = await startDaemon()
  try {
    // No banner: a plain long-running process under the claude preset, exactly like a
    // real agent whose CLI never emitted a resume link.
    const created = await fetch(`${daemon.baseURL}/api/v1/sessions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${daemon.token}` },
      body: JSON.stringify({
        kind: "agent",
        preset: "claude",
        command: "/bin/sh",
        args: ["-c", "sleep 120"],
        cwd: "/tmp",
        cols: 80,
        rows: 24,
      }),
    })
    expect(created.status).toBe(201)

    await page.goto(`${daemon.baseURL}/?token=${daemon.token}`)
    await expect(page.getByRole("listitem").first()).toBeVisible({ timeout: 15_000 })

    const { dataDir, port } = daemon
    await stopDaemon(daemon, { keepDataDir: true })
    daemon = await startDaemon({ dataDir, port })

    await page.reload()
    await page.getByRole("tab", { name: /^Closed/ }).click()
    const lostRow = page.getByRole("listitem").first()
    await expect(lostRow).toContainText("lost", { timeout: 15_000 })

    await lostRow.getByRole("button", { name: /Resume/ }).click()
    await expect(page.getByLabel("Preset")).toHaveValue("claude")
    await expect(page.getByLabel(/Resume session ID/)).toHaveValue("")
    // The launcher says what Launch will do rather than leaving a blank field
    // looking like the action failed.
    await expect(page.getByText(/list this folder's conversations/)).toBeVisible()

    await page.screenshot({ path: "e2e/.tmp/restore-without-id.png", fullPage: true })
  } finally {
    await stopDaemon(daemon)
  }
})
