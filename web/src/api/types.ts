// Types mirroring docs/04-api-protocol.md. Kept as plain interfaces/unions --
// no runtime validation library; the daemon is the only producer and this is
// a same-origin, same-version-in-practice client (docs/04-api-protocol.md
// "version skew" is a native-app concern, not a browser-SPA one).

export type SessionKind = "shell" | "agent" | "command"
// "lost" (docs/05-persistence.md#restart-recovery): a session that was
// running/closing when the daemon last stopped, recovered as terminal on
// the next startup with no clean exit code -- a historical row only, never
// a live Session's own state.
export type SessionState = "running" | "closing" | "exited" | "lost"

export interface Session {
  id: string
  kind: SessionKind
  preset: string | null
  command: string
  args: string[]
  cwd: string
  state: SessionState
  pid: number | null
  cols: number
  rows: number
  output_bytes: number
  created_at_ms: number
  started_at_ms: number | null
  exited_at_ms: number | null
  exit_code: number | null
  lost_reason: string | null
  controller: string | null
  subscribers: number
  /** D3 (docs/04-api-protocol.md#get-apiv1sessions). */
  last_bell_ms: number | null
  idle_since_ms: number | null
  /** The agent's own most recent terminal-title update, e.g. Claude Code's "✳ Say hello". */
  title: string | null
  /** A Claude Code resumable-conversation id (`session_<id>`), if this session's output ever carried one. */
  claude_resume_id: string | null
}

export interface SessionsResponse {
  sessions: Session[]
}

export interface CreateSessionRequest {
  kind: SessionKind
  preset?: string
  command?: string
  args?: string[]
  cwd: string
  cols: number
  rows: number
  env?: Record<string, string>
}

export interface CreateSessionResponse {
  id: string
  state: SessionState
  pid: number | null
  output_offset: number
}

export interface Preset {
  id: string
  label: string
  command: string
  args: string[]
  icon: string
  /**
   * Daemon-side spawn detail, mirrored here only so this file stays a faithful
   * copy of `GET /api/v1/presets` (docs/04-api-protocol.md#get-apiv1presets) --
   * the UI has nothing to do with it. Optional because a daemon older than the
   * field omits it entirely.
   */
  login_shell?: boolean
  /**
   * How this agent is told to resume a previous conversation, e.g.
   * `["--resume"]`. Empty (or absent, on a daemon older than the field)
   * means it has no resume story, and no Resume action is offered. This is
   * the only thing the UI knows about any agent's capabilities -- there is
   * deliberately no preset id anywhere in the resume path.
   */
  resume_args?: string[]
}

export interface PresetsResponse {
  presets: Preset[]
}

/** `GET /api/v1/browse` (docs/04-api-protocol.md#get-apiv1browse). Subdirectories only. */
export interface BrowseEntry {
  name: string
  path: string
}

export interface BrowseResponse {
  /** The resolved path actually listed -- not necessarily identical to the request's `path`. */
  path: string
  /** `null` only at an actual filesystem root. */
  parent: string | null
  entries: BrowseEntry[]
}

/** One pinned launcher directory (docs/19-locations.md#pins). */
export interface Pin {
  path: string
  pinned_at_ms: number
}

/** `GET /api/v1/locations/pins`. Newest pin first. */
export interface PinsResponse {
  pins: Pin[]
}

/** `POST /api/v1/ws-ticket` (docs/06-security.md#token-on-the-websocket-upgrade). */
export interface WsTicketResponse {
  ticket: string
  expires_in: number
}

export interface HealthResponse {
  status: string
  version: string
  api_versions: string[]
  capabilities: string[]
  device_id?: string
  device_name?: string
  platform?: string
  /** The daemon's own home directory -- lets the UI collapse a session's `cwd` to `~/...`. */
  home_dir?: string
  /**
   * The release tag `<data_dir>/web/current` points at
   * (docs/18-ui-upgrades.md#telling-the-client). Absent or null when the
   * daemon is serving its embedded bundle or an explicit `--web-dist`, so a
   * change in it means the UI on disk was flipped under this tab.
   */
  ui_version?: string | null
  pid?: number
  uptime_ms?: number
  sessions_running?: number
}

export interface ApiErrorBody {
  error: string
  message: string
}

/** Thrown by `api/api.ts` for any non-2xx response. */
export class ApiError extends Error {
  status: number
  code: string
  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = "ApiError"
    this.status = status
    this.code = code
  }
}

// -- WebSocket protocol (docs/04-api-protocol.md#websocket-protocol) --

export interface ReadyFrame {
  type: "ready"
  session_id: string
  replay_from: number
  next_offset: number
  truncated: boolean
  log_capped_at: number | null
  cols: number
  rows: number
  control: boolean
  controller: string | null
}

export interface ControlGrantedFrame {
  type: "control_granted"
}

export interface ControlRevokedFrame {
  type: "control_revoked"
  to: string
  client_id: string
}

export interface ResizedFrame {
  type: "resized"
  cols: number
  rows: number
}

export interface ExitFrame {
  type: "exit"
  code: number | null
  final_offset: number
}

export interface ErrorFrame {
  type: "error"
  code: string
  message?: string
  next_offset?: number
}

export type ServerFrame =
  | ReadyFrame
  | ControlGrantedFrame
  | ControlRevokedFrame
  | ResizedFrame
  | ExitFrame
  | ErrorFrame

export type ClientMessage =
  | { type: "resize"; cols: number; rows: number }
  | { type: "claim_control" }
  | { type: "release_control" }

/** `stream.ts`'s own connection-state machine -- not part of the wire protocol. */
export type StreamState = "connecting" | "replaying" | "live" | "reconnecting" | "closed"

// --- Passkey login (docs/17-passkey-login.md#api-surface) ---

/** `GET /auth/status`. Drives which of three login screens renders. */
export interface AuthStatus {
  /** Whether this origin has a usable relying-party ID at all. */
  passkey_supported: boolean
  rp_id: string | null
  /** Scoped to `rp_id` -- enrolment at another origin does not count here. */
  enrolled: boolean
  /** The `localhost` URL to switch to, sent only when unsupported. */
  token_url_hint: string | null
}

/** A started ceremony. `options` goes straight to `navigator.credentials`. */
export interface CeremonyStart<T> {
  challenge_id: string
  options: T
}

/** What a successful assertion mints. Used exactly like the master token. */
export interface LoginResponse {
  token: string
  expires_at_ms: number
  session_id: string
}

export interface PasskeySummary {
  id: string
  rp_id: string
  label: string
  created_at_ms: number
  last_used_ms: number | null
}

export interface AuthSessionSummary {
  id: string
  passkey_id: string
  label: string
  created_at_ms: number
  expires_at_ms: number
  last_seen_ms: number
  /** True for the session making the request. */
  current: boolean
}
