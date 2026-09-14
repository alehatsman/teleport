// Typed HTTP client for `/api/v1` (docs/04-api-protocol.md#http-surface).
// Same-origin always -- Vite's dev proxy makes `:5173` look same-origin too
// (docs/09-frontend.md#dev-workflow) -- so this never needs a base URL.

import { clearSessionToken, getToken, hasSessionToken } from "./identity"
import {
  ApiError,
  type ApiErrorBody,
  type AuthSessionSummary,
  type AuthStatus,
  type BrowseResponse,
  type CeremonyStart,
  type CreateSessionRequest,
  type CreateSessionResponse,
  type HealthResponse,
  type LoginResponse,
  type PasskeySummary,
  type PresetsResponse,
  type Session,
  type SessionsResponse,
  type WsTicketResponse,
} from "./types"
import type { JsonCreationOptions, JsonRequestOptions } from "./webauthn"

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const token = getToken()
  const headers = new Headers(init?.headers)
  if (token) headers.set("Authorization", `Bearer ${token}`)
  if (init?.body) headers.set("Content-Type", "application/json")

  const response = await fetch(`/api/v1${path}`, { ...init, headers })

  if (response.status === 204) return undefined as T

  const isJson = response.headers.get("content-type")?.includes("application/json")
  const payload = isJson ? await response.json() : undefined

  if (!response.ok) {
    // A 401 while holding a passkey session means that session is revoked or
    // expired. Drop it so the next call falls back to the master token (if
    // there is one) and the app returns to the login screen -- a dead
    // session token otherwise shadows a credential that still works.
    if (response.status === 401 && hasSessionToken()) clearSessionToken()
    const body = payload as ApiErrorBody | undefined
    throw new ApiError(
      response.status,
      body?.error ?? "unknown",
      body?.message ?? response.statusText
    )
  }
  return payload as T
}

// The daemon's two setup failures each have one fix, and the raw message
// ("Origin or Host rejected") gave no clue what it was. Say the fix. Shared
// by Sessions.svelte (list/refresh errors) and SessionLauncher.svelte
// (launch errors) -- promoted here rather than duplicated per UI.md rule 11
// once a second consumer needed it.
export function describeError(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === "unauthorized") {
      return "Sign in again — this browser's credential is no longer valid."
    }
    if (e.code === "bad_origin") {
      return `${e.message}. Add ${window.location.origin} to allowed_origins in teleportd's config.toml and restart it.`
    }
    return e.message
  }
  return e instanceof Error ? e.message : String(e)
}

export function health(): Promise<HealthResponse> {
  return request("/health")
}

export function listSessions(): Promise<SessionsResponse> {
  return request("/sessions")
}

export function createSession(body: CreateSessionRequest): Promise<CreateSessionResponse> {
  return request("/sessions", { method: "POST", body: JSON.stringify(body) })
}

export function getSession(id: string): Promise<Session> {
  return request(`/sessions/${id}`)
}

/** `purge: true` also deletes the on-disk log; it's the only way a session leaves the list. */
export function deleteSession(id: string, purge = false): Promise<void> {
  return request(`/sessions/${id}${purge ? "?purge=true" : ""}`, { method: "DELETE" })
}

/** Raw log bytes (`Content-Type: application/octet-stream`) for the "scrollback truncated" link. */
export async function getLog(
  id: string,
  range?: { from?: number; to?: number }
): Promise<Uint8Array> {
  const token = getToken()
  const headers = new Headers()
  if (token) headers.set("Authorization", `Bearer ${token}`)
  const params = new URLSearchParams()
  if (range?.from !== undefined) params.set("from", String(range.from))
  if (range?.to !== undefined) params.set("to", String(range.to))
  const query = params.toString()

  const response = await fetch(`/api/v1/sessions/${id}/log${query ? `?${query}` : ""}`, { headers })
  if (!response.ok) throw new ApiError(response.status, "log_error", response.statusText)
  return new Uint8Array(await response.arrayBuffer())
}

export function listPresets(): Promise<PresetsResponse> {
  return request("/presets")
}

export function browse(path?: string): Promise<BrowseResponse> {
  return request(`/browse${path ? `?path=${encodeURIComponent(path)}` : ""}`)
}

/**
 * Trades the master bearer token (sent the normal way, as `Authorization`)
 * for a short-lived, single-use ticket scoped to `sessionId` --
 * `stream.ts` puts *that* in the WebSocket URL instead
 * (docs/06-security.md#token-on-the-websocket-upgrade, mitigation 2).
 */
export function createWsTicket(sessionId: string): Promise<WsTicketResponse> {
  return request("/ws-ticket", { method: "POST", body: JSON.stringify({ session_id: sessionId }) })
}

/** Builds the `ws://…/api/v1/sessions/{id}/stream` URL `stream.ts` connects to. */
export function streamUrl(id: string, query: URLSearchParams): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:"
  return `${proto}//${window.location.host}/api/v1/sessions/${id}/stream?${query.toString()}`
}

// --- Passkey login (docs/17-passkey-login.md#api-surface) ---

/**
 * Unauthenticated on purpose: the SPA calls this before it holds any
 * credential, to pick between the passkey screen, the setup screen, and the
 * token fallback (docs/09-frontend.md#credential-precedence-and-the-login-screen).
 */
export function authStatus(): Promise<AuthStatus> {
  return request("/auth/status")
}

/** Authenticated -- the bootstrap gate. Requires the token on a fresh daemon. */
export function registerStart(userName?: string): Promise<CeremonyStart<JsonCreationOptions>> {
  return request("/auth/passkey/register/start", {
    method: "POST",
    body: JSON.stringify({ user_name: userName ?? null }),
  })
}

export function registerFinish(
  challengeId: string,
  credential: unknown,
  label?: string
): Promise<PasskeySummary> {
  return request("/auth/passkey/register/finish", {
    method: "POST",
    body: JSON.stringify({ challenge_id: challengeId, credential, label: label ?? null }),
  })
}

export function loginStart(): Promise<CeremonyStart<JsonRequestOptions>> {
  return request("/auth/passkey/login/start", { method: "POST", body: JSON.stringify({}) })
}

export function loginFinish(
  challengeId: string,
  credential: unknown,
  label?: string
): Promise<LoginResponse> {
  return request("/auth/passkey/login/finish", {
    method: "POST",
    body: JSON.stringify({ challenge_id: challengeId, credential, label: label ?? null }),
  })
}

export function listPasskeys(): Promise<PasskeySummary[]> {
  return request("/auth/passkeys")
}

export function renamePasskey(id: string, label: string): Promise<void> {
  return request(`/auth/passkeys/${id}`, { method: "PATCH", body: JSON.stringify({ label }) })
}

export function deletePasskey(id: string): Promise<void> {
  return request(`/auth/passkeys/${id}`, { method: "DELETE" })
}

export function listAuthSessions(): Promise<AuthSessionSummary[]> {
  return request("/auth/sessions")
}

export function deleteAuthSession(id: string): Promise<void> {
  return request(`/auth/sessions/${id}`, { method: "DELETE" })
}

export function logout(): Promise<void> {
  return request("/auth/logout", { method: "POST", body: JSON.stringify({}) })
}
