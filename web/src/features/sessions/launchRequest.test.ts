import { describe, expect, it } from "vitest"
import { buildLaunchRequest } from "./launchRequest"

const base = {
  selectedPreset: "",
  customCommand: "/bin/sh",
  cwd: "",
  homeDir: null,
  resumeSessionId: "",
  resumeRequested: false,
}

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
  it("passes --resume only to the claude preset, trimmed", () => {
    expect(
      buildLaunchRequest({ ...base, selectedPreset: "claude", resumeSessionId: " abc " })
    ).toEqual({
      kind: "agent",
      preset: "claude",
      cwd: "/",
      cols: 120,
      rows: 36,
      args: ["--resume", "abc"],
    })
    expect(
      buildLaunchRequest({ ...base, selectedPreset: "claude", resumeSessionId: "  " })
    ).not.toHaveProperty("args")
    expect(
      buildLaunchRequest({ ...base, selectedPreset: "codex", resumeSessionId: "abc" })
    ).not.toHaveProperty("args")
  })
  // The restore path: Claude Code no longer emits the OSC 8 link the id is
  // read from, so "Resume" on a closed session usually has nothing to pass.
  // Bare `--resume` opens its picker for that folder -- still a resume, and
  // never `--continue`, which would silently pick one conversation for
  // every session restored out of the same repo.
  it("resumes through the picker when no id is known", () => {
    expect(
      buildLaunchRequest({ ...base, selectedPreset: "claude", resumeRequested: true })
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
        resumeSessionId: "session_abc",
        resumeRequested: true,
      }).args
    ).toEqual(["--resume", "session_abc"])
  })
  it("never resumes a non-claude preset, however the launcher was opened", () => {
    expect(
      buildLaunchRequest({ ...base, selectedPreset: "codex", resumeRequested: true })
    ).not.toHaveProperty("args")
  })
})
