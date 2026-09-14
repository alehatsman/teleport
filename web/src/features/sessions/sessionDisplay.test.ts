import { describe, expect, it } from "vitest"
import type { Preset, Session } from "@/api/types"
import {
  BELL_RECENCY_MS,
  canResume,
  displayAge,
  displayCwd,
  displayOutcome,
  displayTitle,
  filterSessions,
  needsAttention,
  outcomeFailed,
  recentCwds,
  resumablePresetIds,
  stateTone,
  viewerStatus,
} from "./sessionDisplay"

function session(over: Partial<Session> = {}): Session {
  return {
    id: "s1",
    kind: "shell",
    preset: null,
    command: "sh",
    args: [],
    cwd: "/home/a/proj",
    state: "running",
    pid: 1,
    cols: 80,
    rows: 24,
    output_bytes: 0,
    created_at_ms: 1_000,
    started_at_ms: 1_000,
    exited_at_ms: null,
    exit_code: null,
    lost_reason: null,
    controller: null,
    subscribers: 0,
    last_bell_ms: null,
    idle_since_ms: null,
    title: null,
    claude_resume_id: null,
    ...over,
  }
}

describe("stateTone", () => {
  it("maps running/lost to a tone and the rest to gray", () => {
    expect(stateTone("running")).toBe("success")
    expect(stateTone("lost")).toBe("warning")
    expect(stateTone("closing")).toBeNull()
    expect(stateTone("exited")).toBeNull()
  })
})

describe("filterSessions", () => {
  const all = [
    session({ id: "a", state: "running", command: "claude", cwd: "/home/a/teleport" }),
    session({ id: "b", state: "closing", command: "sh", cwd: "/home/a/codefort" }),
    session({ id: "c", state: "exited", command: "claude", cwd: "/home/a/teleport" }),
    session({ id: "d", state: "lost", command: "sh", cwd: "/tmp" }),
  ]
  it("splits active from closed", () => {
    expect(filterSessions(all, "active", "").map((s) => s.id)).toEqual(["a", "b"])
    expect(filterSessions(all, "closed", "").map((s) => s.id)).toEqual(["c", "d"])
  })
  it("matches command or absolute cwd, case-insensitive, trimmed", () => {
    expect(filterSessions(all, "active", "  CLAUDE ").map((s) => s.id)).toEqual(["a"])
    expect(filterSessions(all, "closed", "/home/a").map((s) => s.id)).toEqual(["c"])
    expect(filterSessions(all, "closed", "nothing")).toEqual([])
  })
})

describe("recentCwds", () => {
  it("dedupes, orders most-recent-first, skips empty, caps at 8", () => {
    const sessions = [
      session({ cwd: "/old", created_at_ms: 1 }),
      session({ cwd: "/new", created_at_ms: 3 }),
      session({ cwd: "/old", created_at_ms: 2 }),
      session({ cwd: "", created_at_ms: 9 }),
    ]
    expect(recentCwds(sessions)).toEqual(["/new", "/old"])
    const many = Array.from({ length: 12 }, (_, i) => session({ cwd: `/d${i}`, created_at_ms: i }))
    expect(recentCwds(many)).toHaveLength(8)
    expect(recentCwds(many)[0]).toBe("/d11")
  })
})

describe("needsAttention", () => {
  const now = 100_000
  it("is never set for a session that is not running", () => {
    expect(needsAttention(session({ state: "exited", idle_since_ms: 1, kind: "agent" }), now)).toBe(
      false
    )
  })
  it("flags an idle agent but not an idle shell", () => {
    expect(needsAttention(session({ kind: "agent", idle_since_ms: 1 }), now)).toBe(true)
    expect(needsAttention(session({ kind: "shell", idle_since_ms: 1 }), now)).toBe(false)
  })
  it("flags a recent bell for any kind, against the passed clock", () => {
    const recent = session({ last_bell_ms: now - BELL_RECENCY_MS + 1 })
    const stale = session({ last_bell_ms: now - BELL_RECENCY_MS })
    expect(needsAttention(recent, now)).toBe(true)
    expect(needsAttention(stale, now)).toBe(false)
  })
})

describe("displayAge", () => {
  const now = 10_000_000_000
  it("is coarse: now, minutes, hours up to 48, then days", () => {
    expect(displayAge(now - 59_000, now)).toBe("now")
    expect(displayAge(now - 60_000, now)).toBe("1m")
    expect(displayAge(now - 59 * 60_000, now)).toBe("59m")
    expect(displayAge(now - 60 * 60_000, now)).toBe("1h")
    expect(displayAge(now - 47 * 3_600_000, now)).toBe("47h")
    expect(displayAge(now - 48 * 3_600_000, now)).toBe("2d")
    expect(displayAge(now + 5_000, now)).toBe("now") // clock skew never goes negative
  })
})

describe("displayOutcome / outcomeFailed", () => {
  it("names the exit, and only a non-zero or lost one is a failure", () => {
    expect(displayOutcome(session({ state: "running" }))).toBeNull()
    expect(displayOutcome(session({ state: "exited", exit_code: null }))).toBe("exited")
    expect(displayOutcome(session({ state: "exited", exit_code: 0 }))).toBe("exit 0")
    expect(displayOutcome(session({ state: "exited", exit_code: 3 }))).toBe("exit 3")
    expect(displayOutcome(session({ state: "lost" }))).toBe("lost")
    expect(outcomeFailed(session({ state: "exited", exit_code: 0 }))).toBe(false)
    expect(outcomeFailed(session({ state: "exited", exit_code: null }))).toBe(false)
    expect(outcomeFailed(session({ state: "exited", exit_code: 1 }))).toBe(true)
    expect(outcomeFailed(session({ state: "lost" }))).toBe(true)
  })
})

describe("displayTitle", () => {
  it("prefers the live title over the command", () => {
    expect(displayTitle(session({ title: "✳ Fix login bug", command: "claude" }), "s1")).toBe(
      "✳ Fix login bug"
    )
  })

  it("falls back to the command with no title", () => {
    expect(displayTitle(session({ title: null, command: "claude" }), "s1")).toBe("claude")
  })

  it("falls back to the id with no session record", () => {
    expect(displayTitle(null, "s1")).toBe("s1")
  })
})

describe("displayCwd", () => {
  it("collapses only at a real path boundary", () => {
    expect(displayCwd("/Users/a/proj", null)).toBe("/Users/a/proj")
    expect(displayCwd("/Users/a", "/Users/a")).toBe("~")
    expect(displayCwd("/Users/a/proj", "/Users/a")).toBe("~/proj")
    expect(displayCwd("/Users/a-test/proj", "/Users/a")).toBe("/Users/a-test/proj")
  })
})

describe("viewerStatus", () => {
  it("reports the socket while the record is unknown or the process lives", () => {
    expect(viewerStatus(null, "connecting")).toEqual({
      ended: false,
      unsettled: true,
      tone: "warning-strong",
      label: "Connecting",
    })
    expect(viewerStatus(session(), "live")).toEqual({
      ended: false,
      unsettled: false,
      tone: "success",
      label: "Live",
    })
    expect(viewerStatus(session(), "reconnecting").tone).toBe("warning-strong")
    expect(viewerStatus(session(), "closed")).toEqual({
      ended: false,
      unsettled: false,
      tone: null,
      label: "Closed",
    })
  })
  it("lets the record win once the process ended, whatever the socket says", () => {
    expect(viewerStatus(session({ state: "exited", exit_code: 3 }), "connecting")).toEqual({
      ended: true,
      unsettled: false,
      tone: null,
      label: "Exited (code 3)",
    })
    expect(viewerStatus(session({ state: "exited", exit_code: null }), "live").label).toBe("Exited")
    expect(viewerStatus(session({ state: "lost" }), "live")).toEqual({
      ended: true,
      unsettled: false,
      tone: "warning",
      label: "Lost",
    })
  })
})

describe("resumablePresetIds / canResume", () => {
  function preset(id: string, resume_args?: string[]): Preset {
    return { id, label: id, command: id, args: [], icon: id, resume_args }
  }

  // The rule that used to be `preset === "claude"` spelled out in two
  // components. Which agents resume is presets.toml's business now.
  it("selects presets by their resume_args, not by id", () => {
    const ids = resumablePresetIds([
      preset("claude", ["--resume"]),
      preset("codex", []),
      preset("shell"),
      preset("someagent", ["session", "resume"]),
    ])
    expect([...ids].sort()).toEqual(["claude", "someagent"])
  })

  it("treats an absent resume_args as not resumable, for an older daemon", () => {
    expect(resumablePresetIds([preset("claude")]).size).toBe(0)
  })

  it("offers resume only for a closed session on a resumable preset", () => {
    const resumable = new Set(["claude"])
    expect(canResume(session({ state: "exited", preset: "claude" }), resumable)).toBe(true)
    expect(canResume(session({ state: "lost", preset: "claude" }), resumable)).toBe(true)
    // Still running: there is nothing to resume, the session is right there.
    expect(canResume(session({ state: "running", preset: "claude" }), resumable)).toBe(false)
    expect(canResume(session({ state: "exited", preset: "codex" }), resumable)).toBe(false)
    // A raw command or shell session carries no preset at all.
    expect(canResume(session({ state: "exited", preset: null }), resumable)).toBe(false)
  })
})
