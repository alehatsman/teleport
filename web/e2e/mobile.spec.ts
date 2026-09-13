// Smoke pass at a real mobile viewport/UA (the "mobile" project in
// playwright.config.ts, devices['iPhone 14']) -- this product is mobile-
// first (docs/09-frontend.md#mobile), it doesn't get a desktop-only test
// suite with mobile as an afterthought. Not a full duplicate of the other
// specs: just the things that are actually viewport/UA-shaped.

import { expect, test } from "./fixtures"

test("the session list never scrolls horizontally on a phone-sized viewport", async ({
  authedPage: page,
}) => {
  const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth)
  const viewportWidth = await page.evaluate(() => window.innerWidth)
  expect(scrollWidth).toBeLessThanOrEqual(viewportWidth + 1)
})

test("launching and typing into a session works with a real mobile UA", async ({
  authedPage: page,
}) => {
  // Not a bare getByRole("button", {name: "New session"}): the empty-state
  // CTA and the mobile FAB share the same accessible name -- scope to the
  // list's header landmark to pick the one that's always there.
  await page.getByRole("banner").getByRole("button", { name: "New session" }).click()
  await page.getByLabel("Preset").selectOption({ label: "Shell" })
  await page.getByLabel("Working directory").fill("/tmp")
  await page.getByRole("button", { name: "Launch" }).click()

  await expect(page.getByText("Controlling", { exact: true })).toBeVisible()

  const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth)
  const viewportWidth = await page.evaluate(() => window.innerWidth)
  expect(scrollWidth).toBeLessThanOrEqual(viewportWidth + 1)

  const marker = `e2e-mobile-${Date.now()}`
  await page.locator(".xterm-helper-textarea").click()
  await page.keyboard.type(`echo ${marker}`)
  await page.keyboard.press("Enter")

  // Not toHaveCount(2) (session-lifecycle.spec.ts's assertion, at desktop
  // width): a phone-narrow terminal wraps the longer "echo <marker>" input
  // line across two rows, splitting that occurrence in the DOM -- only the
  // unwrapped output line is reliably one exact-text match here.
  await expect(page.getByText(marker).last()).toBeVisible()
})
