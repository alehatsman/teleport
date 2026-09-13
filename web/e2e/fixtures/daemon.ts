// Spawns and tears down a real teleportd for e2e tests -- never a mock
// WebSocket server (docs/10-testing.md#web-e2e-playwright: prove things
// against the real primitive, same as the Rust integration tests do).
//
// Two callers: global-setup.ts (one shared instance for most specs) and
// reconnect-and-lost.spec.ts (its own dedicated instance, since that spec
// deliberately kills the daemon and must not disturb specs running in
// parallel against the shared one).

import { type ChildProcess, spawn } from "node:child_process"
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

export type DaemonHandle = {
  proc: ChildProcess
  port: number
  token: string
  dataDir: string
  baseURL: string
}

const READY_TIMEOUT_MS = 15_000
const POLL_INTERVAL_MS = 100

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

async function waitForFile(path: string, timeoutMs: number): Promise<string> {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    try {
      const contents = readFileSync(path, "utf8").trim()
      if (contents) return contents
    } catch {
      // Not written yet -- keep polling. ENOENT is the expected steady
      // state until the daemon finishes binding.
    }
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${path}`)
    await sleep(POLL_INTERVAL_MS)
  }
}

async function waitForHealth(baseURL: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    try {
      const res = await fetch(`${baseURL}/api/v1/health`)
      if (res.ok) return
    } catch {
      // Connection refused until the listener is up -- expected, keep polling.
    }
    if (Date.now() > deadline) throw new Error(`${baseURL}/api/v1/health never came up`)
    await sleep(POLL_INTERVAL_MS)
  }
}

/**
 * Starts a fresh teleportd against an ephemeral port and a throwaway data
 * dir, and waits until it actually answers `/health` -- never a fixed sleep
 * (same wait-for-ready idiom as scripts/mobile-dev/up.sh).
 *
 * `TELEPORTD_BIN` (CI: a pre-built binary path) is used when set; otherwise
 * falls back to `cargo run -p teleportd --` so a plain local `npm run
 * test:e2e` also works, matching up.sh's own fallback-friendly style. One
 * code path for both, not a CI/local fork.
 *
 * Auth stays on (the real default) -- see docs/06-security.md#authentication
 * and web/e2e/fixtures/index.ts's `authedPage`, which exercises the real
 * `?token=` capture flow rather than routing around it.
 *
 * `opts.dataDir` + `opts.port` let reconnect-and-lost.spec.ts restart a
 * daemon against the *same* data dir and port a page already has open --
 * docs/01-architecture.md#the-crash-boundary's "recovered as lost" path only
 * fires when the metadata (SQLite + logs) survives, and the already-open
 * page's WebSocket can only ever reconnect to the port it was loaded from.
 */
export async function startDaemon(
  opts: { dataDir?: string; port?: number } = {}
): Promise<DaemonHandle> {
  const dataDir = opts.dataDir ?? mkdtempSync(join(tmpdir(), "teleport-e2e-"))
  const webDist = join(import.meta.dirname, "..", "..", "dist")
  const portFile = join(dataDir, "port")
  if (existsSync(portFile)) {
    // Reusing a data dir: the daemon only ever *writes* this file, so a
    // stale one from the previous run would otherwise satisfy the "does it
    // exist yet" wait below the instant this new process starts, before it
    // has bound anything.
    rmSync(portFile)
  }

  // Destructured, not `process.env.TELEPORTD_BIN` -- `process.env` is an
  // index signature and the ts-quality baseline's
  // noPropertyAccessFromIndexSignature rejects dot access on one (same
  // reason vite.config.ts destructures it up top).
  const { TELEPORTD_BIN: bin } = process.env
  const [command, baseArgs] = bin
    ? [bin, [] as string[]]
    : ["cargo", ["run", "--quiet", "-p", "teleportd", "--"]]

  const proc = spawn(
    command,
    [
      ...baseArgs,
      "--listen",
      `127.0.0.1:${opts.port ?? 0}`,
      "--data-dir",
      dataDir,
      "--web-dist",
      webDist,
    ],
    // `detached: true` makes this process its own process-group leader
    // (POSIX only -- fine here, Windows is a different lane, see
    // agent-lane-macos memory). Without it, killing the `cargo run` wrapper
    // leaves the actual teleportd grandchild it spawned orphaned and still
    // holding the port -- stopDaemon() below kills the whole group instead.
    { stdio: "pipe", detached: true }
  )
  // Surface a startup failure immediately instead of only via the health
  // poll's generic timeout -- a stack trace in stderr is much faster to
  // diagnose than "never came up".
  let stderr = ""
  proc.stderr?.on("data", (chunk) => {
    stderr += chunk.toString()
  })
  const exitedEarly = new Promise<never>((_, reject) => {
    proc.once("exit", (code) => {
      if (code !== 0) reject(new Error(`teleportd exited early (code ${code}): ${stderr}`))
    })
  })

  const port = await Promise.race([
    waitForFile(portFile, READY_TIMEOUT_MS).then(Number),
    exitedEarly,
  ])
  if (opts.port !== undefined && port !== opts.port) {
    throw new Error(
      `teleportd fell back to an ephemeral port (wanted ${opts.port}, got ${port}) -- ` +
        "the port likely wasn't free yet after the previous instance's shutdown"
    )
  }
  const token = readFileSync(join(dataDir, "token"), "utf8").trim()
  const baseURL = `http://127.0.0.1:${port}`
  await Promise.race([waitForHealth(baseURL, READY_TIMEOUT_MS), exitedEarly])

  return { proc, port, token, dataDir, baseURL }
}

export async function stopDaemon(
  handle: DaemonHandle,
  opts: { keepDataDir?: boolean } = {}
): Promise<void> {
  const pid = handle.proc.pid
  await new Promise<void>((resolve) => {
    handle.proc.once("exit", () => resolve())
    // Negative pid: kill the whole process group (see the `detached` note
    // above), not just the `cargo run` wrapper.
    if (pid !== undefined) process.kill(-pid, "SIGKILL")
    else handle.proc.kill("SIGKILL")
  })
  // `keepDataDir`: reconnect-and-lost.spec.ts is about to restart against
  // this same dir so the session recovers as `lost`, not vanishes.
  if (!opts.keepDataDir) rmSync(handle.dataDir, { recursive: true, force: true })
}
