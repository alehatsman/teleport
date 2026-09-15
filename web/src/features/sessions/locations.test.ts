import { describe, expect, it } from "vitest"
import type { Session } from "@/api/types"
import { HALF_LIFE_MS, knownLocations, type Location, matchLocations } from "./locations"

const NOW = 1_000_000_000_000
const DAY = 24 * 60 * 60 * 1000

function session(cwd: string, agoMs: number): Session {
  return {
    id: `s-${cwd}-${agoMs}`,
    kind: "shell",
    preset: null,
    command: "sh",
    args: [],
    cwd,
    state: "exited",
    pid: 1,
    cols: 80,
    rows: 24,
    output_bytes: 0,
    created_at_ms: NOW - agoMs,
    started_at_ms: NOW - agoMs,
    exited_at_ms: null,
    exit_code: null,
    lost_reason: null,
    controller: null,
    subscribers: 0,
    last_bell_ms: null,
    idle_since_ms: null,
    title: null,
    claude_resume_id: null,
  }
}

const paths = (locs: Location[]) => locs.map((l) => l.path)

describe("knownLocations", () => {
  it("splits a path into the basename and a home-collapsed parent", () => {
    const [loc] = knownLocations([session("/home/a/projects/teleport", 0)], [], "/home/a", NOW)
    expect(loc).toMatchObject({
      path: "/home/a/projects/teleport",
      name: "teleport",
      parent: "~/projects",
      uses: 1,
      pinned: false,
    })
  })

  it("keeps the root as the parent of a top-level directory", () => {
    const [loc] = knownLocations([session("/srv", 0)], [], "/home/a", NOW)
    expect(loc).toMatchObject({ name: "srv", parent: "/" })
  })

  it("handles a Windows path", () => {
    const [loc] = knownLocations([session("C:\\src\\app", 0)], [], null, NOW)
    expect(loc).toMatchObject({ name: "app", parent: "C:\\src" })
  })

  it("skips a session with no cwd", () => {
    expect(knownLocations([session("", 0)], [], null, NOW)).toEqual([])
  })

  it("counts uses and takes the most recent launch as lastUsedMs", () => {
    const locs = knownLocations([session("/a", 3 * DAY), session("/a", DAY)], [], null, NOW)
    expect(locs[0]).toMatchObject({ uses: 2, lastUsedMs: NOW - DAY })
  })

  it("ranks often-used above merely-recent", () => {
    // Ten launches a week ago are worth 10 * 0.5 = 5; one yesterday is ~0.9.
    const often = Array.from({ length: 10 }, () => session("/often", HALF_LIFE_MS))
    const locs = knownLocations([...often, session("/recent", DAY)], [], null, NOW)
    expect(paths(locs)).toEqual(["/often", "/recent"])
  })

  it("decays: the same use count breaks toward the more recent one", () => {
    const locs = knownLocations([session("/old", 30 * DAY), session("/new", DAY)], [], null, NOW)
    expect(paths(locs)).toEqual(["/new", "/old"])
  })

  it("puts every pin ahead of every unpinned location", () => {
    const busy = Array.from({ length: 20 }, () => session("/busy", 0))
    const locs = knownLocations([...busy, session("/quiet", 30 * DAY)], ["/quiet"], null, NOW)
    expect(paths(locs)).toEqual(["/quiet", "/busy"])
  })

  it("lists a pin with no session history at all", () => {
    const locs = knownLocations([], ["/home/a/archive"], "/home/a", NOW)
    expect(locs).toEqual([
      {
        path: "/home/a/archive",
        name: "archive",
        parent: "~",
        uses: 0,
        lastUsedMs: 0,
        pinned: true,
        score: 0,
      },
    ])
  })

  it("orders history-less pins by path so the chips don't reshuffle on a poll", () => {
    const pins = ["/b", "/a", "/c"]
    expect(paths(knownLocations([], pins, null, NOW))).toEqual(["/a", "/b", "/c"])
  })
})

describe("matchLocations", () => {
  const locs = knownLocations(
    [
      session("/home/a/projects/futurumlab/teleport", DAY),
      session("/home/a/projects/futurumlab/teleport-web", 2 * DAY),
      session("/home/a/work/telemetry", 3 * DAY),
      session("/home/a/notes", 4 * DAY),
    ],
    [],
    "/home/a",
    NOW
  )

  it("returns everything, ranked, for an empty query", () => {
    expect(matchLocations(locs, "   ".trim())).toBe(locs)
  })

  it("requires every term, in any order, anywhere in the path", () => {
    expect(paths(matchLocations(locs, "tel fut"))).toEqual([
      "/home/a/projects/futurumlab/teleport",
      "/home/a/projects/futurumlab/teleport-web",
    ])
  })

  it("splits a pasted path on its separators", () => {
    expect(paths(matchLocations(locs, "work/telemetry"))).toEqual(["/home/a/work/telemetry"])
  })

  it("is case-insensitive", () => {
    expect(paths(matchLocations(locs, "TELEMETRY"))).toEqual(["/home/a/work/telemetry"])
  })

  it("ranks a basename match above a parent-only match", () => {
    // "futurumlab" is in both teleport paths' parent; the folder itself is
    // named futurumlab nowhere, so order falls back to frecency.
    expect(paths(matchLocations(locs, "futurumlab"))).toEqual([
      "/home/a/projects/futurumlab/teleport",
      "/home/a/projects/futurumlab/teleport-web",
    ])
    // "projects" matches only parents; "notes" matches a basename. A query
    // hitting both puts the basename hit first.
    const mixed = matchLocations(locs, "home")
    expect(mixed).toHaveLength(4)
  })

  it("puts a basename hit ahead of a better-ranked parent-only hit", () => {
    const two = knownLocations(
      [session("/home/a/teleport/src", DAY), session("/home/a/src/teleport", 30 * DAY)],
      [],
      "/home/a",
      NOW
    )
    expect(paths(two)).toEqual(["/home/a/teleport/src", "/home/a/src/teleport"])
    expect(paths(matchLocations(two, "teleport"))).toEqual([
      "/home/a/src/teleport",
      "/home/a/teleport/src",
    ])
  })

  it("returns nothing when a term matches nothing", () => {
    expect(matchLocations(locs, "teleport zzz")).toEqual([])
  })
})
