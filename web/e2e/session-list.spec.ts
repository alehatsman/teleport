// Runs against the shared daemon (fixtures/index.ts), which specs in this
// suite run against concurrently -- every assertion here is scoped to a
// unique per-test marker (never the raw Active/Closed counts, which reflect
// every session on the shared daemon and would be flaky under parallel
// execution). See docs/10-testing.md#web-e2e-playwright.

import { mkdirSync } from "node:fs"
import { createExitingSession, createShellSession, expect, test } from "./fixtures"

// The daemon validates `cwd` exists before spawning (POST /api/v1/sessions
// -> 422 otherwise) -- a bare unique string under /tmp isn't enough, it has
// to be a real directory. The basename doubles as the search-query marker:
// unique per test and per worker, so parallel runs never collide.
function uniqueCwd(label: string): string {
  const dir = `/tmp/e2e-${label}-${test.info().workerIndex}-${Date.now()}`
  mkdirSync(dir, { recursive: true })
  return dir
}

test.describe("session list", () => {
  test("search filters by working directory", async ({ authedPage: page, apiRequest }) => {
    const cwd = uniqueCwd("search")
    await createShellSession(apiRequest, { cwd })

    await page.getByRole("searchbox", { name: "Search sessions" }).fill(cwd)

    await expect(page.getByText(cwd)).toBeVisible()
  })

  test("search excludes sessions that don't match", async ({ authedPage: page, apiRequest }) => {
    const cwd = uniqueCwd("nomatch")
    await createShellSession(apiRequest, { cwd })

    const query = `${cwd}-does-not-exist`
    await page.getByRole("searchbox", { name: "Search sessions" }).fill(query)

    // Not `getByText(cwd)).toHaveCount(0)`: the empty-state message itself
    // echoes the query back ('No active sessions match "..."'), and `query`
    // contains `cwd` as a substring -- that message would be a false-positive
    // match for a plain "is cwd still visible anywhere" check.
    await expect(page.getByText(`No active sessions match "${query}"`)).toBeVisible()
  })

  test("closed sessions can be bulk-deleted", async ({ authedPage: page, apiRequest }) => {
    const cwd = uniqueCwd("delete")
    await Promise.all([
      createExitingSession(apiRequest, cwd),
      createExitingSession(apiRequest, cwd),
    ])

    await page.getByRole("tab", { name: /^Closed/ }).click()
    await page.getByRole("searchbox", { name: "Search sessions" }).fill(cwd)
    // The 3s list-poll (Sessions.svelte) plus process-exit latency -- not
    // instant, but never a fixed sleep: this keeps retrying the query until
    // both rows land or the timeout gives up for real.
    await expect(page.getByText(cwd)).toHaveCount(2, { timeout: 10_000 })

    await page.getByRole("button", { name: "Select" }).click()
    // Scoped to `filteredSessions` by the app itself (Sessions.svelte's
    // toggleSelectAll) -- safe against the shared daemon's other closed
    // sessions from specs running in parallel, because the search above
    // already narrowed the list to just these two.
    await page.getByRole("checkbox", { name: "Select all" }).check()
    await expect(page.getByText("2 selected")).toBeVisible()

    page.once("dialog", (dialog) => dialog.accept())
    await page.getByRole("button", { name: "Delete (2)" }).click()

    // Same reasoning as the "excludes" test above: the empty-state message
    // would itself satisfy a plain "cwd no longer appears" count check.
    await expect(page.getByText(`No closed sessions match "${cwd}"`)).toBeVisible()
  })
})
