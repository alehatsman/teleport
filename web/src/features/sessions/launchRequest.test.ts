import { describe, expect, it } from "vitest"
import { buildLaunchRequest } from "./launchRequest"

const base = {
  selectedPreset: "",
  customCommand: "/bin/sh",
  cwd: "",
  homeDir: null,
  resumeArgs: [] as string[],
  resumeSessionId: "",
  resumeRequested: false,
}

// What the daemon ships for the `claude` preset. Named for the capability,
// not the vendor -- nothing in launchRequest.ts knows which agent this is.
const RESUMES = ["--resume"]

describe("buildLaunchRequest", () => {
  it("falls back cwd -> homeDir -> /", () => {
    expect(buildLaunchRequest(base).cwd).toBe("/")
    expect(buildLaunchRequest({ ...base, homeDir: "/home/a" }).cwd).toBe("/home/a")
    expect(buildLaunchRequest({ ...base, homeDir: "/home/a", cwd: "/x" }).cwd).toBe("/x")
  })
  it("builds a shell request when no preset is chosen", () => {
    expect(buildLaunchRequest({ ...base, customCommand: "zsh" })).toEqual({
      kind: "shell",
      command: "zsh",
      cwd: "/",
      cols: 120,
      rows: 36,
    })
  })
  it("appends a trimmed id to the preset's own resume args", () => {
    expect(
      buildLaunchRequest({
        ...base,
        selectedPreset: "claude",
        resumeArgs: RESUMES,
        resumeSessionId: " abc ",
      })
    ).toEqual({
      kind: "agent",
      preset: "claude",
      cwd: "/",
      cols: 120,
      rows: 36,
      args: ["--resume", "abc"],
    })
    expect(
      buildLaunchRequest({
        ...base,
        selectedPreset: "claude",
        resumeArgs: RESUMES,
        resumeSessionId: "  ",
      })
    ).not.toHaveProperty("args")
  })

  // The rule that used to be `selectedPreset === "claude"`. A preset the
  // daemon says cannot resume never gets a resume argv, however the launcher
  // was opened or what stale id the form is still holding.
  it("never builds resume args for a preset with none", () => {
    expect(
      buildLaunchRequest({ ...base, selectedPreset: "codex", resumeSessionId: "abc" })
    ).not.toHaveProperty("args")
    expect(
      buildLaunchRequest({ ...base, selectedPreset: "codex", resumeRequested: true })
    ).not.toHaveProperty("args")
  })

  // Multi-arg resume: nothing assumes a single flag, so a preset whose agent
  // resumes via a subcommand works without touching this file.
  it("supports a multi-argument resume form", () => {
    expect(
      buildLaunchRequest({
        ...base,
        selectedPreset: "someagent",
        resumeArgs: ["session", "resume"],
        resumeSessionId: "xyz",
      }).args
    ).toEqual(["session", "resume", "xyz"])
  })
  // The restore path: Claude Code no longer emits the OSC 8 link the id is
  // read from, so "Resume" on a closed session usually has nothing to pass.
  // Bare `--resume` opens its picker for that folder -- still a resume, and
  // never `--continue`, which would silently pick one conversation for
  // every session restored out of the same repo.
  it("resumes through the picker when no id is known", () => {
    expect(
      buildLaunchRequest({
        ...base,
        selectedPreset: "claude",
        resumeArgs: RESUMES,
        resumeRequested: true,
      })
    ).toEqual({
      kind: "agent",
      preset: "claude",
      cwd: "/",
      cols: 120,
      rows: 36,
      args: ["--resume"],
    })
  })
  it("prefers a known id over the picker", () => {
    expect(
      buildLaunchRequest({
        ...base,
        selectedPreset: "claude",
        resumeArgs: RESUMES,
        resumeSessionId: "session_abc",
        resumeRequested: true,
      }).args
    ).toEqual(["--resume", "session_abc"])
  })
})
