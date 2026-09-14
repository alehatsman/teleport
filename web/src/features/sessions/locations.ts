// A "location" is a directory teleport can launch a session in: one the
// session history already knows, or one that was pinned
// (docs/18-locations.md). Pure helpers, no reactive state, `now` always
// passed in (web/CLAUDE.md) -- the ranking is a function of the clock, so a
// helper reading Date.now() itself would silently freeze inside a $derived.

import type { Session } from "@/api/types"
import { displayCwd } from "./sessionDisplay"

export type Location = {
  /** Absolute, exactly as the daemon recorded/resolved it. */
  path: string
  /** Basename -- the part the reader actually recognizes. */
  name: string
  /** Everything above `name`, home-collapsed to "~". Empty at a root. */
  parent: string
  /** Sessions launched here, within whatever window the daemon still retains. */
  uses: number
  /** 0 for a pin with no session history behind it. */
  lastUsedMs: number
  pinned: boolean
  /** Frecency, see below. Only meaningful relative to another location's. */
  score: number
}

// Each launch is worth 1 point, halving every week: the sum is frequency,
// the decay is recency, and one number orders the list. A folder opened ten
// times last week outranks one opened once yesterday; a folder untouched for
// a month sinks without ever being deleted. Deliberately not a bucketed
// "frecency" table -- there is nothing here to tune.
export const HALF_LIFE_MS = 7 * 24 * 60 * 60 * 1000

/** How many locations the launcher shows inline before you have to open the picker. */
export const LOCATION_CHIPS_MAX = 8

// Both separators: the daemon runs on Windows too (docs/03-pty-layer.md), so
// a cwd can be "C:\src\app" just as easily as "/src/app".
function splitPath(path: string): { name: string; parent: string } {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"))
  if (cut < 0) return { name: path, parent: "" }
  const name = path.slice(cut + 1)
  // A trailing separator ("/src/app/") leaves an empty basename -- fall back
  // to the whole path rather than rendering a chip with no label.
  if (!name) return { name: path, parent: "" }
  // cut === 0 is "/app": the parent is the root itself, not "".
  return { name, parent: cut === 0 ? "/" : path.slice(0, cut) }
}

/**
 * Every directory worth offering, best first: pinned ahead of unpinned, then
 * by frecency. `pins` may name a directory no session ever ran in (that is the
 * point of pinning) and a session's cwd may not be pinned; both end up here.
 */
export function knownLocations(
  sessions: Session[],
  pins: string[],
  homeDir: string | null,
  now: number
): Location[] {
  const stats = new Map<string, { uses: number; lastUsedMs: number; score: number }>()
  for (const s of sessions) {
    if (!s.cwd) continue
    const prev = stats.get(s.cwd) ?? { uses: 0, lastUsedMs: 0, score: 0 }
    stats.set(s.cwd, {
      uses: prev.uses + 1,
      lastUsedMs: Math.max(prev.lastUsedMs, s.created_at_ms),
      score: prev.score + 0.5 ** ((now - s.created_at_ms) / HALF_LIFE_MS),
    })
  }

  const pinned = new Set(pins)
  for (const path of pinned) {
    if (!stats.has(path)) stats.set(path, { uses: 0, lastUsedMs: 0, score: 0 })
  }

  return [...stats.entries()]
    .map(([path, stat]) => {
      const { name, parent } = splitPath(path)
      return {
        path,
        name,
        parent: displayCwd(parent, homeDir),
        uses: stat.uses,
        lastUsedMs: stat.lastUsedMs,
        pinned: pinned.has(path),
        score: stat.score,
      }
    })
    .sort(
      (a, b) =>
        Number(b.pinned) - Number(a.pinned) ||
        b.score - a.score ||
        b.lastUsedMs - a.lastUsedMs ||
        // Last resort so the order is total: two pins with no history are
        // otherwise tied on every field, and an unstable order would reshuffle
        // the chips on every poll.
        a.path.localeCompare(b.path)
    )
}

/**
 * Filter by a typed query. Deliberately dumb: every whitespace- or
 * separator-delimited term must appear somewhere in the path, so "tel fut"
 * finds "~/projects/futurumlab/teleport". No fuzzy matching -- a scoring
 * heuristic nobody can predict is worse than one that misses.
 */
export function matchLocations(locations: Location[], query: string): Location[] {
  const terms = query
    .toLowerCase()
    .split(/[\s/\\]+/)
    .filter(Boolean)
  if (terms.length === 0) return locations

  // Matched in the basename first: typing "teleport" means the folder called
  // teleport, not the twelve folders that merely live under it.
  const byName: Location[] = []
  const byParent: Location[] = []
  for (const loc of locations) {
    const path = loc.path.toLowerCase()
    if (!terms.every((t) => path.includes(t))) continue
    const name = loc.name.toLowerCase()
    if (terms.some((t) => name.includes(t))) byName.push(loc)
    else byParent.push(loc)
  }
  // Both halves keep `locations`' own ranking, so pins and frecency still
  // decide the order within each.
  return [...byName, ...byParent]
}
