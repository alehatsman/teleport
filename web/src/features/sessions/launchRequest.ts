// The launcher form's fields -> one CreateSessionRequest. Pure, so the
// preset/shell branch and the resume-id rule are testable without the form.

import type { CreateSessionRequest } from "@/api/types"

export type LaunchFields = {
  /** "" means custom shell command. */
  selectedPreset: string
  customCommand: string
  cwd: string
  homeDir: string | null
  resumeSessionId: string
}

// Launch geometry is fixed; the controller's first fit-to-viewport resizes
// it the moment the view opens (docs/09-frontend.md#geometry).
const LAUNCH_COLS = 120
const LAUNCH_ROWS = 36

export function buildLaunchRequest(f: LaunchFields): CreateSessionRequest {
  const cwd = f.cwd || f.homeDir || "/"
  if (!f.selectedPreset) {
    return { kind: "shell", command: f.customCommand, cwd, cols: LAUNCH_COLS, rows: LAUNCH_ROWS }
  }
  // Only claude actually understands `--resume`; the field itself is hidden
  // for any other preset, but the trim-and-check happens here too so a
  // stale value left over from switching presets mid-launcher session can
  // never leak into an unrelated command's argv.
  const resumeId = f.selectedPreset === "claude" ? f.resumeSessionId.trim() : ""
  return {
    kind: "agent",
    preset: f.selectedPreset,
    cwd,
    cols: LAUNCH_COLS,
    rows: LAUNCH_ROWS,
    ...(resumeId ? { args: ["--resume", resumeId] } : {}),
  }
}
