// Typed HTTP client for `/api/v1` (docs/04-api-protocol.md#http-surface).
// Same-origin always -- Vite's dev proxy makes `:5173` look same-origin too
// (docs/09-frontend.md#dev-workflow) -- so this never needs a base URL.

import { getToken } from "./identity";
import { ApiError, type ApiErrorBody, type CreateSessionRequest, type CreateSessionResponse, type HealthResponse, type PresetsResponse, type Session, type SessionsResponse } from "./types";

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const token = getToken();
  const headers = new Headers(init?.headers);
  if (token) headers.set("Authorization", `Bearer ${token}`);
  if (init?.body) headers.set("Content-Type", "application/json");

  const response = await fetch(`/api/v1${path}`, { ...init, headers });

  if (response.status === 204) return undefined as T;

  const isJson = response.headers.get("content-type")?.includes("application/json");
  const payload = isJson ? await response.json() : undefined;

  if (!response.ok) {
    const body = payload as ApiErrorBody | undefined;
    throw new ApiError(response.status, body?.error ?? "unknown", body?.message ?? response.statusText);
  }
  return payload as T;
}

export function health(): Promise<HealthResponse> {
  return request("/health");
}

export function listSessions(): Promise<SessionsResponse> {
  return request("/sessions");
}

export function createSession(body: CreateSessionRequest): Promise<CreateSessionResponse> {
  return request("/sessions", { method: "POST", body: JSON.stringify(body) });
}

export function getSession(id: string): Promise<Session> {
  return request(`/sessions/${id}`);
}

/** `purge: true` also deletes the on-disk log; it's the only way a session leaves the list. */
export function deleteSession(id: string, purge = false): Promise<void> {
  return request(`/sessions/${id}${purge ? "?purge=true" : ""}`, { method: "DELETE" });
}

/** Raw log bytes (`Content-Type: application/octet-stream`) for the "scrollback truncated" link. */
export async function getLog(id: string, range?: { from?: number; to?: number }): Promise<Uint8Array> {
  const token = getToken();
  const headers = new Headers();
  if (token) headers.set("Authorization", `Bearer ${token}`);
  const params = new URLSearchParams();
  if (range?.from !== undefined) params.set("from", String(range.from));
  if (range?.to !== undefined) params.set("to", String(range.to));
  const query = params.toString();

  const response = await fetch(`/api/v1/sessions/${id}/log${query ? `?${query}` : ""}`, { headers });
  if (!response.ok) throw new ApiError(response.status, "log_error", response.statusText);
  return new Uint8Array(await response.arrayBuffer());
}

export function listPresets(): Promise<PresetsResponse> {
  return request("/presets");
}

/** Builds the `ws://…/api/v1/sessions/{id}/stream` URL `stream.ts` connects to. */
export function streamUrl(id: string, query: URLSearchParams): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.host}/api/v1/sessions/${id}/stream?${query.toString()}`;
}
