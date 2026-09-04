// Client identity and the bearer token (docs/09-frontend.md#client-identity-and-token).
// `client_id` is not a credential -- it just lets a dropped controller resume
// its own lease and names the controller in everyone else's UI. The token is
// the credential; it comes from the `?token=` the daemon prints at startup.

const CLIENT_ID_KEY = "teleport.client_id";
const CLIENT_NAME_KEY = "teleport.client_name";
const TOKEN_KEY = "teleport.token";

// `crypto` is exposed only in a secure context (HTTPS, or a localhost
// origin). Over plain http://<lan-ip> -- the --i-know-what-im-doing path --
// it is undefined and randomUUID() throws before the app renders. Degrade,
// don't die.
function newClientId(): string {
  return (
    globalThis.crypto?.randomUUID?.() ?? `c-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`
  );
}

function defaultClientName(): string {
  const ua = navigator.userAgent;
  const browser = /Edg\//.test(ua) ? "Edge" : /Chrome\//.test(ua) ? "Chrome" : /Firefox\//.test(ua) ? "Firefox" : /Safari\//.test(ua) ? "Safari" : "Browser";
  const platform = /iPhone|iPad/.test(ua) ? "iOS" : /Android/.test(ua) ? "Android" : /Mac OS X/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /Linux/.test(ua) ? "Linux" : "";
  return platform ? `${browser} on ${platform}` : browser;
}

function readOrCreate(key: string, create: () => string): string {
  try {
    const existing = localStorage.getItem(key);
    if (existing) return existing;
    const created = create();
    localStorage.setItem(key, created);
    return created;
  } catch {
    // Private browsing / storage disabled: fall back to a per-load value
    // rather than crashing the app.
    return create();
  }
}

export const CLIENT_ID = readOrCreate(CLIENT_ID_KEY, newClientId);
export const CLIENT_NAME = readOrCreate(CLIENT_NAME_KEY, defaultClientName);

export function getToken(): string | null {
  try {
    return localStorage.getItem(TOKEN_KEY);
  } catch {
    return null;
  }
}

/**
 * Did this client last hold the control lease on this session? Checked on
 * mount so a reopened tab asks to *resume* control (`mode=control`, which
 * never preempts -- docs/09-frontend.md#streamts) instead of silently
 * dropping back to observer just because the page reloaded and a fresh
 * `SessionStream`'s `wantControl` starts false. Cleared the moment the
 * server says otherwise, so a stale flag can't outlive the grace window by
 * much or survive someone else taking over.
 */
export function wasControlling(sessionId: string): boolean {
  try {
    return localStorage.getItem(`teleport.controlling.${sessionId}`) === "1";
  } catch {
    return false;
  }
}

export function setControlling(sessionId: string, controlling: boolean): void {
  try {
    if (controlling) localStorage.setItem(`teleport.controlling.${sessionId}`, "1");
    else localStorage.removeItem(`teleport.controlling.${sessionId}`);
  } catch {
    // Best-effort only -- worst case a reopened tab asks to resume control
    // it no longer needs to, which is a harmless no-op server-side.
  }
}

export function setToken(token: string): void {
  try {
    localStorage.setItem(TOKEN_KEY, token);
  } catch {
    // Nothing we can do without storage; the session will keep asking for
    // `?token=` on every load, which is degraded but not broken.
  }
}

/**
 * Captures `?token=…` from the URL on first load, persists it, and strips it
 * from the address bar so it never sits in history or leaks through
 * `Referer` (docs/06-security.md#token-on-the-websocket-upgrade).
 * Call once, from `main.ts`, before anything renders.
 */
export function captureTokenFromUrl(): void {
  const url = new URL(window.location.href);
  const token = url.searchParams.get("token");
  if (!token) return;
  setToken(token);
  url.searchParams.delete("token");
  window.history.replaceState({}, "", url.pathname + (url.search || "") + url.hash);
}
