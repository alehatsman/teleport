import { expect, test } from "./fixtures"

test.describe("session lifecycle", () => {
  test("launching a shell session attaches with control and echoes typed input", async ({
    authedPage: page,
  }) => {
    // Not a bare getByRole("button", {name: "New session"}): the empty-state
    // CTA and the mobile FAB share the same accessible name -- scope to the
    // list's header landmark to pick the one that's always there.
    await page.getByRole("banner").getByRole("button", { name: "New session" }).click()
    await page.getByLabel("Preset").selectOption({ label: "Shell" })
    await page.getByLabel("Working directory").fill("/tmp")
    await page.getByRole("button", { name: "Launch" }).click()

    // A freshly-launched session's lease is unheld by construction
    // (docs/04-api-protocol.md#control-lease) -- the launcher's own client
    // gets control on the very next connect, no explicit claim needed.
    await expect(page).toHaveURL(/#\/sessions\//)
    await expect(page.getByText("Live", { exact: true })).toBeVisible()
    await expect(page.getByText("Controlling", { exact: true })).toBeVisible()

    // xterm.js exposes no accessible role for its canvas-rendered terminal
    // (a real gap in the library, not this app) -- the hidden input-capture
    // textarea it renders for IME/mobile input is the one place a real
    // keystroke can land, same as a real click focuses it for a human.
    const marker = `e2e-marker-${Date.now()}`
    await page.locator(".xterm-helper-textarea").click()
    await page.keyboard.type(`echo ${marker}`)
    await page.keyboard.press("Enter")

    // Two rows contain the marker: the PTY's own echo of the typed command,
    // and its actual stdout below -- the second is what proves the command
    // really executed, not just that keystrokes reached the terminal.
    await expect(page.getByText(marker)).toHaveCount(2)
    await expect(page.getByText(marker).last()).toBeVisible()
  })
})
