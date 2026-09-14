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
  /**
   * The launcher was opened by "Resume" on a closed session, not by "New
   * session". With no `resumeSessionId` in hand this still launches
   * `claude --resume`, which opens Claude Code's own conversation picker
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
  // Only claude actually understands `--resume`; the field itself is hidden
  // for any other preset, but the trim-and-check happens here too so a
  // stale value left over from switching presets mid-launcher session can
  // never leak into an unrelated command's argv.
  const isClaude = f.selectedPreset === "claude"
  const resumeId = isClaude ? f.resumeSessionId.trim() : ""
  // Bare `--resume`: Claude Code lists the conversations it has for this
  // folder and lets one be picked. That's the honest restore when no id is
  // known -- which, as of Claude Code 2.1.x, is every session, since the
  // OSC 8 link `claude_resume_id` is read from is no longer emitted
  // (docs/04-api-protocol.md#get-apiv1sessions: not a versioned contract).
  //
  // Deliberately not `--continue`. That resumes the *most recent*
  // conversation in the directory without asking, so restoring four agents
  // that were all working in the same repo -- the normal shape of a fleet
  // of them -- would silently point all four at one conversation.
  const resumeArgs = resumeId ? ["--resume", resumeId] : f.resumeRequested ? ["--resume"] : []
  return {
    kind: "agent",
    preset: f.selectedPreset,
    cwd,
    cols: LAUNCH_COLS,
    rows: LAUNCH_ROWS,
    ...(isClaude && resumeArgs.length > 0 ? { args: resumeArgs } : {}),
  }
}
