// Shared Playwright fixtures for every spec except reconnect-and-lost.spec.ts
// (which owns its own daemon instance and doesn't use the shared one this
// file reads). See docs/10-testing.md#web-e2e-playwright.

import { readFileSync } from "node:fs"
import { type APIRequestContext, test as base, expect, type Page, request } from "@playwright/test"
import { STATE_FILE } from "../global-setup"

export type DaemonInfo = {
  port: number
  token: string
  dataDir: string
  baseURL: string
  pid: number
}

function readDaemonInfo(): DaemonInfo {
  return JSON.parse(readFileSync(STATE_FILE, "utf8"))
}

type Fixtures = {
  daemon: DaemonInfo
  /**
   * A page that has already done the one real `?token=...` navigation
   * (web/src/api/identity.ts#captureTokenFromUrl) -- the actual onboarding
   * flow, exercised by every test rather than routed around with a stored
   * localStorage value. Everything after this fixture resolves is a normal
   * same-origin navigation.
   */
  authedPage: Page
  /** A request context pre-authed the normal way (`Authorization: Bearer`), for fast test-setup calls that aren't themselves the thing under test. */
  apiRequest: APIRequestContext
}

export const test = base.extend<Fixtures>({
  // Playwright parses this parameter's literal source text to find fixture
  // dependencies -- an empty destructuring pattern is how a fixture
  // declares it needs none, not a mistake to fix.
  // biome-ignore lint/correctness/noEmptyPattern: required by Playwright, see above
  daemon: async ({}, use) => {
    await use(readDaemonInfo())
  },
  authedPage: async ({ page, daemon }, use) => {
    await page.goto(`${daemon.baseURL}/?token=${daemon.token}`)
    await use(page)
  },
  apiRequest: async ({ daemon }, use) => {
    const ctx = await request.newContext({
      baseURL: daemon.baseURL,
      extraHTTPHeaders: { Authorization: `Bearer ${daemon.token}` },
    })
    await use(ctx)
    await ctx.dispose()
  },
})

export { expect }

/**
 * Creates a session via the real REST API -- for tests whose subject isn't
 * the launcher form itself, so they don't pay for clicking through it just
 * to get a session to point at.
 */
export async function createSession(
  api: APIRequestContext,
  body: Record<string, unknown>
): Promise<string> {
  const res = await api.post("/api/v1/sessions", { data: { cols: 80, rows: 24, ...body } })
  expect(res.ok(), `POST /api/v1/sessions: ${res.status()} ${await res.text()}`).toBeTruthy()
  const created = await res.json()
  return created.id as string
}

/** A long-lived shell -- for control-lease / typed-input tests. */
export function createShellSession(api: APIRequestContext, opts: { cwd?: string } = {}) {
  return createSession(api, { kind: "shell", preset: "shell", cwd: opts.cwd ?? "/tmp" })
}

/** Exits almost immediately -- for tests about the Closed list, not about a live process. */
export function createExitingSession(api: APIRequestContext, cwd: string) {
  return createSession(api, { kind: "command", command: "/bin/echo", args: ["e2e"], cwd })
}
