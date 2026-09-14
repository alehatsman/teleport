import { describe, expect, it } from "vitest"
import type { AuthStatus, PasskeySummary } from "@/api/types"
import {
  chooseScreen,
  describeLastUsed,
  groupByRp,
  shouldOfferSetup,
  unsupportedReason,
} from "./authDisplay"

function status(overrides: Partial<AuthStatus> = {}): AuthStatus {
  return {
    passkey_supported: true,
    rp_id: "localhost",
    enrolled: true,
    token_url_hint: null,
    ...overrides,
  }
}

describe("chooseScreen", () => {
  it("never interrupts a client that already holds a credential", () => {
    // The master token is a permanently supported way in, not a legacy path.
    expect(chooseScreen(status({ enrolled: false }), true, true)).toBe("app")
    expect(chooseScreen(null, true, false)).toBe("app")
  })

  it("offers the passkey button only when one is enrolled for this origin", () => {
    expect(chooseScreen(status(), false, true)).toBe("passkey-login")
    expect(chooseScreen(status({ enrolled: false }), false, true)).toBe("token-only")
  })

  it("falls back to the token path when the origin cannot do passkeys", () => {
    // 127.0.0.1: an RP ID cannot be an IP.
    expect(chooseScreen(status({ passkey_supported: false }), false, true)).toBe("token-only")
  })

  it("falls back to the token path when the browser cannot do passkeys", () => {
    // A supported origin is not enough if navigator.credentials is missing.
    expect(chooseScreen(status(), false, false)).toBe("token-only")
  })

  it("falls back to the token path while status is still unknown", () => {
    expect(chooseScreen(null, false, true)).toBe("token-only")
  })
})

describe("shouldOfferSetup", () => {
  it("nudges a signed-in user with nothing enrolled here", () => {
    expect(shouldOfferSetup(status({ enrolled: false }), true, true)).toBe(true)
  })

  it("stays quiet once this origin has a passkey", () => {
    expect(shouldOfferSetup(status(), true, true)).toBe(false)
  })

  it("stays quiet when enrollment could not succeed anyway", () => {
    expect(shouldOfferSetup(status({ passkey_supported: false }), true, true)).toBe(false)
    expect(shouldOfferSetup(status({ enrolled: false }), true, false)).toBe(false)
  })

  it("never nudges someone who is not signed in", () => {
    // Enrollment requires an existing credential; offering it otherwise
    // would be an invitation to a 401.
    expect(shouldOfferSetup(status({ enrolled: false }), false, true)).toBe(false)
  })
})

describe("unsupportedReason", () => {
  it("points at the localhost URL rather than just refusing", () => {
    const message = unsupportedReason(
      status({ passkey_supported: false, token_url_hint: "http://localhost:7337" }),
      true
    )
    expect(message).toContain("http://localhost:7337")
  })

  it("blames the browser when the browser is what is missing", () => {
    expect(unsupportedReason(status(), false)).toContain("This browser")
  })

  it("still says something useful with no hint at all", () => {
    expect(unsupportedReason(null, true)).toContain("token")
  })
})

describe("groupByRp", () => {
  const passkey = (id: string, rp_id: string): PasskeySummary => ({
    id,
    rp_id,
    label: id,
    created_at_ms: 0,
    last_used_ms: null,
  })

  it("groups by origin and sorts the origins", () => {
    const grouped = groupByRp([
      passkey("b", "zed.ts.net"),
      passkey("a", "localhost"),
      passkey("c", "localhost"),
    ])
    expect(grouped.map(([rp]) => rp)).toEqual(["localhost", "zed.ts.net"])
    expect(grouped[0]?.[1].map((p) => p.id)).toEqual(["a", "c"])
  })

  it("handles an empty list", () => {
    expect(groupByRp([])).toEqual([])
  })
})

describe("describeLastUsed", () => {
  const now = 1_000_000_000_000

  it("says so when a credential has never been used", () => {
    expect(describeLastUsed(null, now)).toBe("never used")
  })

  it("scales the unit with the age", () => {
    expect(describeLastUsed(now - 30_000, now)).toBe("used just now")
    expect(describeLastUsed(now - 5 * 60_000, now)).toBe("used 5m ago")
    expect(describeLastUsed(now - 3 * 3_600_000, now)).toBe("used 3h ago")
    expect(describeLastUsed(now - 2 * 86_400_000, now)).toBe("used 2d ago")
  })
})
