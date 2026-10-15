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
  await expect(pageA.getByRole("button", { name: "Release control" })).toBeVisible()

  const contextB = await browser.newContext()
  const pageB = await contextB.newPage()
  await pageB.goto(url)
  // mode=control on attach never preempts (docs/04-api-protocol.md#why-
  // attach-must-not-preempt) -- B must land as an observer, attributed to A.
  const takeControlB = pageB.getByRole("button", { name: /Take control/ })
  await expect(takeControlB).toBeVisible()
  await expect(pageB.getByRole("button", { name: "Release control" })).not.toBeVisible()

  // Claims are preemptive: one click, no confirmation dialog.
  await takeControlB.click()
  await expect(pageB.getByRole("button", { name: "Release control" })).toBeVisible()

  // A is notified and drops to observer -- no data loss, no reload needed.
  await expect(pageA.getByText(/Control taken by/)).toBeVisible()
  await expect(pageA.getByRole("button", { name: "Release control" })).not.toBeVisible()
  await expect(pageA.getByRole("button", { name: /Take control/ })).toBeVisible()

  await contextA.close()
  await contextB.close()
})

test("releasing the lease frees it, locally and for the other client", async ({
  browser,
  daemon,
  apiRequest,
}) => {
  const sessionId = await createShellSession(apiRequest)
  const url = `${daemon.baseURL}/?token=${daemon.token}#/sessions/${sessionId}`

  const contextA = await browser.newContext()
  const pageA = await contextA.newPage()
  await pageA.goto(url)
  await pageA.getByRole("button", { name: "Take control" }).click()
  const releaseA = pageA.getByRole("button", { name: "Release control" })
  await expect(releaseA).toBeVisible()

  const contextB = await browser.newContext()
  const pageB = await contextB.newPage()
  await pageB.goto(url)
  // B sees the lease attributed to A while A still holds it.
  await expect(pageB.getByRole("button", { name: /Take control \(from/ })).toBeVisible()

  // The badge is the release button: one click hands the lease back.
  await releaseA.click()
  await expect(releaseA).not.toBeVisible()
  await expect(pageA.getByRole("button", { name: "Take control" })).toBeVisible()

  // The daemon sends no frame for a release, so B learns it from the
  // controller-name poll (docs/09-frontend.md#control-lease-ui): the
  // "(from ...)" attribution drops off once the lease is free.
  await expect(pageB.getByRole("button", { name: "Take control", exact: true })).toBeVisible({
    timeout: 15_000,
  })

  // A reload must not resume the lease A just gave up.
  await pageA.reload()
  await expect(pageA.getByRole("button", { name: /Take control/ })).toBeVisible()
  await expect(pageA.getByRole("button", { name: "Release control" })).not.toBeVisible()

  await contextA.close()
  await contextB.close()
})
