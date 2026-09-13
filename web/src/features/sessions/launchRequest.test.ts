import { describe, expect, it } from "vitest"
import { buildLaunchRequest } from "./launchRequest"

const base = {
  selectedPreset: "",
  customCommand: "/bin/sh",
  cwd: "",
  homeDir: null,
  resumeSessionId: "",
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
})
