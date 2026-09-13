// One shared teleportd for every spec except reconnect-and-lost.spec.ts,
// which owns its own dedicated instance so killing it never disturbs
// whatever else is running in parallel (docs/10-testing.md#web-e2e-playwright).
//
// Written state (port/token/dataDir/pid) is handed to tests via a plain JSON
// file rather than playwright.config.ts's `use.baseURL`: the config module
// is evaluated once, before this setup runs, so it can't know the port yet.
// fixtures/index.ts reads this file instead.

import { mkdirSync, rmSync, writeFileSync } from "node:fs"
import { startDaemon } from "./fixtures/daemon"

export const STATE_DIR = new URL(".tmp/", import.meta.url).pathname
export const STATE_FILE = `${STATE_DIR}daemon.json`

export default async function globalSetup(): Promise<void> {
  rmSync(STATE_DIR, { recursive: true, force: true })
  mkdirSync(STATE_DIR, { recursive: true })

  const daemon = await startDaemon()
  writeFileSync(
    STATE_FILE,
    JSON.stringify({
      port: daemon.port,
      token: daemon.token,
      dataDir: daemon.dataDir,
      baseURL: daemon.baseURL,
      pid: daemon.proc.pid,
    })
  )
}
