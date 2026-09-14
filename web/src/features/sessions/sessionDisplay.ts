// Pure helpers over the Session record: what a row or a viewer header shows
// for a given state. No reactive state, no api/ import (UI.md rule 27:
// helpers with no reactive state live beside the component and are
// unit-testable without mounting anything). `now` is always passed in, never
// read from Date.now() here -- a $derived over a non-reactive clock never
// re-runs, and a pure function that reads the clock is not pure.

import type { Session, SessionState, StreamState } from "@/api/types"
import type { DotTone } from "@/ui/tones"

export const STATE_LABELS: Record<SessionState, string> = {
  running: "Running",
  closing: "Closing",
  exited: "Exited",
  lost: "Lost",
}

/** The list row's dot: green while running, amber when the daemon lost it, gray otherwise. */
export function stateTone(state: SessionState): DotTone {
  if (state === "running") return "success"
  if (state === "lost") return "warning"
  return null
}

export type StatusFilter = "active" | "closed"

/** "Active" is running|closing; "closed" is exited|lost. */
export function isActiveStatus(s: Session): boolean {
  return s.state === "running" || s.state === "closing"
}

// Client-side only -- the full list is already on hand from polling, and a
// session count that ever justified a server-side search endpoint would
// justify pagination first. Matches command or cwd (against the real
// absolute path, not the "~/..." display string -- typing the username you
// already know shouldn't be punished for it).
export function filterSessions(
  sessions: Session[],
  statusFilter: StatusFilter,
  searchQuery: string
): Session[] {
  const byStatus = sessions.filter((s) => isActiveStatus(s) === (statusFilter === "active"))
  const q = searchQuery.trim().toLowerCase()
  if (!q) return byStatus
  return byStatus.filter(
    (s) => s.command.toLowerCase().includes(q) || s.cwd.toLowerCase().includes(q)
  )
}

// M8 (docs/11-mvp-plan.md#m8--agent-presets): recent working directories,
// derived from the session list already on hand -- no new storage/endpoint.
// Most-recent-use-first, deduped, capped so the launcher's chips stay
// scannable.
export const RECENT_CWDS_MAX = 8

export function recentCwds(sessions: Session[]): string[] {
  const latest = new Map<string, number>()
  for (const s of sessions) {
    if (!s.cwd) continue
    const prev = latest.get(s.cwd)
    if (prev === undefined || s.created_at_ms > prev) latest.set(s.cwd, s.created_at_ms)
  }
  return [...latest.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, RECENT_CWDS_MAX)
    .map(([dir]) => dir)
}

// D3 (docs/04-api-protocol.md#get-apiv1sessions): idle_since_ms is already
// a live signal (the daemon clears it the moment output resumes), but
// last_bell_ms never clears server-side -- one bell three hours ago
// shouldn't glow forever. Bound it to a recency window here instead of
// teaching the daemon an "acknowledged" concept for M8.
export const BELL_RECENCY_MS = 2 * 60 * 1000

export function needsAttention(s: Session, now: number): boolean {
  if (s.state !== "running") return false
  // "Went quiet" only means "waiting on you" for an agent. A shell at its
  // prompt is quiet by definition -- flagging every idle shell made the dot
  // light up on every row and mean nothing. A bell still counts for any
  // kind: a process that rang is asking, whatever it is.
  if (s.idle_since_ms !== null && s.kind === "agent") return true
  return s.last_bell_ms !== null && now - s.last_bell_ms < BELL_RECENCY_MS
}

// "3m", "2h", "5d" -- the one glance that separates six identical "sh"
// rows. Deliberately coarse: this is for telling rows apart, not auditing.
export function displayAge(sinceMs: number, now: number): string {
  const s = Math.max(0, Math.floor((now - sinceMs) / 1000))
  if (s < 60) return "now"
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  if (h < 48) return `${h}h`
  return `${Math.floor(h / 24)}d`
}

// What a closed row ended as. "exit 0" is as informative as "exit 3":
// silence here made every closed row look the same.
export function displayOutcome(s: Session): string | null {
  if (s.state === "exited") return s.exit_code === null ? "exited" : `exit ${s.exit_code}`
  if (s.state === "lost") return "lost"
  return null
}

/** A non-zero exit or a lost process: the outcome renders in the danger color. */
export function outcomeFailed(s: Session): boolean {
  return s.state === "lost" || (s.exit_code ?? 0) !== 0
}

// "/Users/aleh/projects/teleport" next to six other rows exactly like it is
// mostly noise -- collapse it to "~/projects/teleport" the way a shell
// prompt would, once we know the daemon's own home dir (homeDir is null
// while loading, on failure, or if the daemon couldn't resolve one --
// either way this is a no-op fallback, never wrong, just less pretty).
// Matches only a real path-segment boundary (homeDir itself, or homeDir +
// "/"), not an unrelated sibling directory that merely starts with the same
// characters (e.g. "/Users/aleh-test").
export function displayCwd(path: string, homeDir: string | null): string {
  if (!homeDir) return path
  if (path === homeDir) return "~"
  if (path.startsWith(`${homeDir}/`)) return `~${path.slice(homeDir.length)}`
  return path
}

// The one primary label for a session, shared by the list row and the
// viewer header: `title` is the agent's own live terminal-title update
// (e.g. "✳ Fix login bug") -- once it exists it's strictly more informative
// than the bare executable name, so it replaces `command` wherever a
// session identifies itself. Falls back to the id for a session record that
// failed to load (Session.svelte's sessionError path).
export function displayTitle(
  session: Pick<Session, "title" | "command"> | null,
  fallback: string
): string {
  return session?.title || session?.command || fallback
}

/** What the viewer header says about one session: record first, socket second. */
export type ViewerStatus = {
  /** The process is gone (exited or lost) -- nothing left to control. */
  ended: boolean
  /** Connection in flux (connecting/reconnecting) while the process lives: amber + pulse. */
  unsettled: boolean
  tone: DotTone
  label: string
}

// A process that ended is a fact about the session, not about our socket.
// The header used to say "Closed" (the connection) after the 4s exit toast
// faded, and the exit code was gone with it. Derive the visible status from
// the session record first, the connection second.
export function viewerStatus(session: Session | null, connection: StreamState): ViewerStatus {
  const ended = session?.state === "exited" || session?.state === "lost"
  const unsettled = !ended && (connection === "reconnecting" || connection === "connecting")
  let label: string
  if (session?.state === "exited") {
    label = session.exit_code === null ? "Exited" : `Exited (code ${session.exit_code})`
  } else if (session?.state === "lost") {
    label = "Lost"
  } else {
    // Capitalized here, not via CSS text-transform: that capitalized every
    // word and turned "Exited (code 3)" into "Exited (Code 3)".
    label = connection.charAt(0).toUpperCase() + connection.slice(1)
  }
  let tone: DotTone = null
  if (session?.state === "lost") tone = "warning"
  else if (unsettled) tone = "warning-strong"
  else if (!ended && connection === "live") tone = "success"
  return { ended, unsettled, tone, label }
}
