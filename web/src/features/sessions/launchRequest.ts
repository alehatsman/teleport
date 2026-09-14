// The launcher form's fields -> one CreateSessionRequest. Pure, so the
// preset/shell branch and the resume rule are testable without the form.

import type { CreateSessionRequest } from "@/api/types"

export type LaunchFields = {
  /** "" means custom shell command. */
  selectedPreset: string
  customCommand: string
  cwd: string
  homeDir: string | null
  /**
   * The selected preset's own `resume_args` (`GET /api/v1/presets`). Empty
   * means this agent has no resume story and no resume argv is ever built --
   * which is why no preset id appears in this file. teleport does not know
   * which harness resumes; `presets.toml` does
   * (docs/04-api-protocol.md#get-apiv1presets).
   */
  resumeArgs: string[]
  resumeSessionId: string
  /**
   * The launcher was opened by "Resume" on a closed session, not by "New
   * session". With no `resumeSessionId` in hand this still sends the bare
   * `resume_args`, which for Claude Code opens its own conversation picker
   * for that folder -- the restore path when no id is known.
   */
  resumeRequested: boolean
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
  // A preset with no `resume_args` cannot resume, so a stale id left over
  // from switching presets mid-launcher-session can never leak into an
  // unrelated command's argv.
  const canResume = f.resumeArgs.length > 0
  const resumeId = canResume ? f.resumeSessionId.trim() : ""
  // With an id: resume that exact conversation. Without one, but asked to
  // restore: the bare args, which for Claude Code means its own picker for
  // the folder. Deliberately never `--continue`-style "most recent", which
  // would point every session restored out of one repo at one conversation
  // -- but that choice now lives in presets.toml, not here.
  let args: string[] = []
  if (canResume && resumeId) args = [...f.resumeArgs, resumeId]
  else if (canResume && f.resumeRequested) args = [...f.resumeArgs]
  return {
    kind: "agent",
    preset: f.selectedPreset,
    cwd,
    cols: LAUNCH_COLS,
    rows: LAUNCH_ROWS,
    ...(args.length > 0 ? { args } : {}),
  }
}
