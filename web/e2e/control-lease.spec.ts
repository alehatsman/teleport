// Exercises the one invariant this product exists to protect: exactly one
// controller, truthfully shown to everyone attached (docs/09-frontend.md
// #control-lease-ui). Two independent browser contexts stand in for two
// real clients -- each gets its own client_id (sessionStorage is per
// context), the same as two different devices would.

import { createShellSession, expect, test } from "./fixtures"

test("claim_control preempts and notifies the client it took over", async ({
  browser,
  daemon,
  apiRequest,
}) => {
  const sessionId = await createShellSession(apiRequest)
  const url = `${daemon.baseURL}/?token=${daemon.token}#/sessions/${sessionId}`

  const contextA = await browser.newContext()
  const pageA = await contextA.newPage()
  await pageA.goto(url)
  // Attached directly by URL, not via the launcher -- the lease starts
  // free, so this is a plain "Take control" claim, not a resume.
  await pageA.getByRole("button", { name: "Take control" }).click()
  await expect(pageA.getByText("Controlling", { exact: true })).toBeVisible()

  const contextB = await browser.newContext()
  const pageB = await contextB.newPage()
  await pageB.goto(url)
  // mode=control on attach never preempts (docs/04-api-protocol.md#why-
  // attach-must-not-preempt) -- B must land as an observer, attributed to A.
  const takeControlB = pageB.getByRole("button", { name: /Take control/ })
  await expect(takeControlB).toBeVisible()
  await expect(pageB.getByText("Controlling", { exact: true })).not.toBeVisible()

  // Claims are preemptive: one click, no confirmation dialog.
  await takeControlB.click()
  await expect(pageB.getByText("Controlling", { exact: true })).toBeVisible()

  // A is notified and drops to observer -- no data loss, no reload needed.
  await expect(pageA.getByText(/Control taken by/)).toBeVisible()
  await expect(pageA.getByText("Controlling", { exact: true })).not.toBeVisible()
  await expect(pageA.getByRole("button", { name: /Take control/ })).toBeVisible()

  await contextA.close()
  await contextB.close()
})
