import { defineConfig, devices } from "@playwright/test"

// Destructured, not `process.env.CI` -- `process.env` is an index signature
// and the ts-quality baseline's noPropertyAccessFromIndexSignature rejects
// dot access on one (same reason vite.config.ts destructures it up top).
const { CI } = process.env

// See docs/10-testing.md#web-e2e-playwright for the daemon-fixture strategy
// and why this doesn't re-run daemon/tests/'s failure-injection matrix.
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  forbidOnly: !!CI,
  retries: CI ? 2 : 0,
  reporter: CI ? [["html", { open: "never" }], ["github"]] : "html",
  globalSetup: "./e2e/global-setup.ts",
  globalTeardown: "./e2e/global-teardown.ts",
  use: {
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
      testIgnore: /mobile\.spec\.ts$/,
    },
    {
      // Only mobile.spec.ts -- the other specs are about protocol/state
      // correctness, not layout, and get no value from a second run at a
      // different viewport. "chromium" runs everything except this file.
      name: "mobile",
      use: { ...devices["iPhone 14"] },
      testMatch: /mobile\.spec\.ts$/,
    },
  ],
})
