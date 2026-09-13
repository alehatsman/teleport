import { readFileSync, rmSync } from "node:fs"
import { STATE_FILE } from "./global-setup"

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

async function waitForExit(pid: number, timeoutMs = 5000): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (pidAlive(pid) && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 50))
  }
}

export default async function globalTeardown(): Promise<void> {
  let state: { pid: number; dataDir: string }
  try {
    state = JSON.parse(readFileSync(STATE_FILE, "utf8"))
  } catch {
    return // global-setup never got far enough to write it
  }

  if (pidAlive(state.pid)) {
    // Negative pid: the whole process group, not just the `cargo run`
    // wrapper -- see fixtures/daemon.ts's `detached: true` note.
    process.kill(-state.pid, "SIGKILL")
    await waitForExit(state.pid)
  }
  rmSync(state.dataDir, { recursive: true, force: true })
}
