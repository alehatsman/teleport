// Pure decisions behind the login screens (web/CLAUDE.md: logic with no
// reactive state is a module, not a component `<script>`).

import type { AuthSessionSummary, AuthStatus, PasskeySummary } from "@/api/types"

/**
 * Which screen the app renders before anything else
 * (docs/09-frontend.md#credential-precedence-and-the-login-screen).
 *
 * - `app` -- a credential is in hand; render the session list as always.
 * - `passkey-login` -- no credential, but this origin can do passkeys and
 *   something is enrolled here.
 * - `token-only` -- no credential and no usable passkey path. The screen
 *   must say *why* and point at the URL that works, never at a button that
 *   cannot succeed.
 */
export type AuthScreen = "app" | "passkey-login" | "token-only"

export function chooseScreen(
  status: AuthStatus | null,
  hasCredential: boolean,
  passkeysUsable: boolean
): AuthScreen {
  // A credential wins outright. Holding the master token is a legitimate,
  // permanently supported way to use this daemon -- it is the recovery path
  // (docs/06-security.md#passkeys-a-second-stage-12-credential) -- so it must
  // never be interrupted by a login screen.
  if (hasCredential) return "app"
  // `status` still loading, or the daemon has passkeys switched off.
  if (!status) return "token-only"
  if (!status.passkey_supported || !passkeysUsable) return "token-only"
  return status.enrolled ? "passkey-login" : "token-only"
}

/**
 * Should the app nudge toward enrolling? Only when a ceremony could actually
 * succeed here and nothing is enrolled for *this* origin yet -- which is the
 * second-origin case as much as the first-run one: a user with a `localhost`
 * passkey arriving at their tailnet hostname has nothing enrolled there.
 */
export function shouldOfferSetup(
  status: AuthStatus | null,
  hasCredential: boolean,
  passkeysUsable: boolean
): boolean {
  return Boolean(hasCredential && passkeysUsable && status?.passkey_supported && !status.enrolled)
}

/**
 * Why this origin cannot do passkeys, phrased as the fix. The RP-ID rule is
 * not something a user can be expected to know, so never surface it as a
 * bare refusal (docs/06-security.md#an-rp-id-is-a-domain-never-an-ip).
 */
export function unsupportedReason(status: AuthStatus | null, passkeysUsable: boolean): string {
  if (!passkeysUsable) {
    return "This browser can't use passkeys here. Sign in with the token teleportd printed at startup."
  }
  const hint = status?.token_url_hint
  return hint
    ? `Passkeys need a hostname, and this address is an IP. Open ${hint} to use one — the daemon is the same, only the address differs.`
    : "Passkeys aren't available on this address. Sign in with the token teleportd printed at startup."
}

/** Groups enrolled credentials by origin, so the settings list can say which
 * addresses are covered and which still need a one-time enrollment. */
export function groupByRp(passkeys: PasskeySummary[]): Array<[string, PasskeySummary[]]> {
  const groups = new Map<string, PasskeySummary[]>()
  for (const passkey of passkeys) {
    const existing = groups.get(passkey.rp_id)
    if (existing) existing.push(passkey)
    else groups.set(passkey.rp_id, [passkey])
  }
  return [...groups.entries()].sort(([a], [b]) => a.localeCompare(b))
}

/** `now` is passed in, never read from the helper (web/CLAUDE.md). */
export function describeLastUsed(lastUsedMs: number | null, now: number): string {
  if (lastUsedMs === null) return "never used"
  const minutes = Math.floor((now - lastUsedMs) / 60_000)
  if (minutes < 1) return "used just now"
  if (minutes < 60) return `used ${minutes}m ago`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `used ${hours}h ago`
  return `used ${Math.floor(hours / 24)}d ago`
}

/**
 * How long a signed-in device has before its session expires, phrased for a
 * revoke list: the question there is "is this still live, and for how long",
 * not a precise timestamp. `now` is passed in, never read here.
 */
export function describeExpiry(expiresAtMs: number, now: number): string {
  const minutes = Math.floor((expiresAtMs - now) / 60_000)
  if (minutes <= 0) return "expired"
  if (minutes < 60) return `expires in ${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 48) return `expires in ${hours}h`
  return `expires in ${Math.floor(hours / 24)}d`
}

/**
 * Signed-in devices, current first and then most-recently-seen. The session
 * making the request is the one a user is most likely to be looking for --
 * both to recognize the list and to avoid revoking by accident -- so it never
 * sorts below a device that happened to check in a moment later.
 */
export function sortAuthSessions(sessions: AuthSessionSummary[]): AuthSessionSummary[] {
  return [...sessions].sort((a, b) => {
    if (a.current !== b.current) return a.current ? -1 : 1
    return b.last_seen_ms - a.last_seen_ms
  })
}
