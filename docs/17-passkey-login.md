# 17 — Passkey login (single owner)

> **Status: implemented**, except the manual browser matrix in
> [Validation](#validation), which needs a real authenticator and a human.
> Steps 1-6 of [Implementation order](#implementation-order) are merged;
> step 7 is not.

## Why this doc exists at all

Three docs currently forbid what this one proposes, and one of them by name:

- [11-mvp-plan.md](11-mvp-plan.md#out-of-scope) lists `custom username/password system`
  as out of scope.
- [06-security.md](06-security.md#authentication): *"A custom username/password system
  is explicitly out of scope for the MVP. When accounts arrive they come from the cloud
  backend — passkeys or OAuth, never hand-rolled passwords."*
- [12-identity-and-connectivity.md](12-identity-and-connectivity.md#the-three-stages)
  gives stage 1/2 exactly one credential: the bearer token in `<data_dir>/token`.

**The resolution:** no password is built. The requirement — "let a human log in with
something they can remember/carry, instead of pasting a 64-hex-char token" — is met with
the mechanism 06 already names as the right one: **passkeys (WebAuthn)**. That keeps the
"never hand-rolled passwords" rule intact, so 11's out-of-scope line stands unamended and
06 gains a stage rather than an exception.

What does change: 06 and 12 currently say the *only* stage-1/2 credential is the token.
Passkeys become a **second** stage-1/2 credential, strictly additive, with the token
remaining the root of trust.

## Goal

One owner of a daemon can sign a browser in with a passkey — Touch ID, Windows Hello, a
hardware key, or a password manager acting as a WebAuthn provider (1Password, iCloud
Keychain, Bitwarden) — instead of pasting the startup token. The session survives daemon
restarts and browser restarts.

## Scope

In:

- A single **owner** identity per daemon. One `user_handle`, one `user_name`, chosen at
  first enrollment. No user table, no second account, no per-user authorization.
- **Many credentials** under that one identity — laptop Touch ID, a phone, a YubiKey, a
  1Password vault entry, the same vault entry again at a second origin. List, label,
  delete.
- Enrollment gated on an already-authenticated caller (the token, or an existing passkey
  session). There is no unauthenticated enrollment path, ever.
- Durable login sessions: an opaque session token, hashed at rest, with expiry and
  explicit revocation.
- A login screen in the web UI, and a "Passkeys" section in settings.

Out:

- Passwords, of any kind, anywhere. The word appears in this doc only to say no.
- Multiple users, ownership of sessions, or any authorization policy change. Every
  authenticated principal continues to authorize identically
  ([12](12-identity-and-connectivity.md#the-principal)).
- Cloud accounts, relay, pairing — still stage 3
  ([14-cloud-backend.md](14-cloud-backend.md)).
- Passkeys for the Tauri shell. It reads `<data_dir>/token` directly and keeps doing so
  (see [Client matrix](#client-matrix)).
- Removing or deprecating the bearer token. It is the recovery path and the root of
  trust; it never goes away.

## The constraint that shapes everything: RP ID

WebAuthn binds a credential to a **relying party ID** — a registrable domain. Two facts
follow, and they drive the UX, the config, and the startup output:

1. **An RP ID cannot be an IP address.** `http://127.0.0.1:7337` is a secure context but
   is *not* WebAuthn-eligible; browsers reject `navigator.credentials.create()` there.
   `localhost` is a registrable domain **and** a trustworthy origin, so
   `http://localhost:7337` works with no TLS.

   → **The startup URL must print `localhost`, not `127.0.0.1`.** The bind address stays
   `127.0.0.1` ([06](06-security.md#listener)); only the printed URL changes. This is the
   one change that touches a user-visible thing unrelated to login.

2. **A credential enrolled at one RP ID does not work at another.** A passkey created at
   `localhost` will not authenticate at `mainpc.tail1234.ts.net`. There is no cross-origin
   passkey, and no configuration fixes this — it is the anti-phishing property, working
   as designed.

   → **Enrollment is per-origin.** Sign in at the tailnet hostname once with the token,
   enroll there too. The UI must say this plainly rather than presenting a broken login.
   A synced provider (1Password, iCloud Keychain) makes both enrollments available on
   every device, which is the reason to prefer one, but it does not merge them into one
   credential.

### Origin/RP matrix

| Origin | RP ID | Passkeys | Why |
|---|---|---|---|
| `http://localhost:<port>` | `localhost` | **yes** | trustworthy origin, registrable domain |
| `http://127.0.0.1:<port>` | — | **no** | RP ID cannot be an IP; token only |
| `https://<host>.ts.net` | `<host>.ts.net` | **yes** | separate enrollment from `localhost` |
| `https://<cf-tunnel-host>` | that host | **yes** | separate enrollment |
| `http://<lan-ip>:<port>` (`--i-know-what-im-doing`) | — | **no** | not a trustworthy origin; token only |
| `tauri://localhost` | — | **no** | custom scheme; shell reads the token file |

The set of RP IDs the daemon will accept is derived, not configured twice: `localhost`
always, plus every entry in `allowed_hosts` ([config](#config)). Building it from
`allowed_hosts` means a user who has already configured remote access
([07](07-remote-access.md)) gets passkey support there with no second setting.

## Client matrix

| Client | Credential | Change |
|---|---|---|
| Browser at `localhost` | passkey session token, falling back to the bearer token | new login screen |
| Browser at a tailnet/CF host | passkey session token (enroll once per host) | new login screen |
| Browser at `127.0.0.1` or a LAN IP | bearer token only | UI explains why, links the `localhost` URL |
| Tauri shell | `<data_dir>/token`, read directly | none |
| CLI / script / native | `Authorization: Bearer <token>` | none |

## Data model

A new SQLite migration ([05-persistence.md](05-persistence.md#migrations) — additive,
appended to `MIGRATIONS`, `user_version` handles the rest). SQLite, not a new JSON file:
this is exactly the metadata the existing store is for, and it gets the same `0600`
treatment as `state.db` already has.

```sql
-- The single owner. Exactly zero or one row; enforced by the CHECK.
CREATE TABLE IF NOT EXISTS owner (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  user_handle   BLOB    NOT NULL,   -- a UUID's 16 bytes; see the note below
  user_name     TEXT    NOT NULL,   -- what the user typed; shown in the authenticator
  created_at_ms INTEGER NOT NULL
);

-- One row per enrolled authenticator, per RP ID.
CREATE TABLE IF NOT EXISTS passkeys (
  id             TEXT    PRIMARY KEY,           -- ULID, the API-facing id
  credential_id  BLOB    NOT NULL,              -- raw WebAuthn credential id
  rp_id          TEXT    NOT NULL,              -- 'localhost', 'x.ts.net', ...
  credential     TEXT    NOT NULL,              -- serialized webauthn-rs Passkey (public key, counter, transports)
  label          TEXT    NOT NULL,              -- user-editable, defaults to the UA guess
  created_at_ms  INTEGER NOT NULL,
  last_used_ms   INTEGER,
  UNIQUE (credential_id, rp_id)
);

-- Login sessions. The token itself is never stored.
CREATE TABLE IF NOT EXISTS auth_sessions (
  id            TEXT    PRIMARY KEY,   -- ULID; this is the `token_id` in the Principal
  token_sha256  BLOB    NOT NULL UNIQUE,
  passkey_id    TEXT    NOT NULL REFERENCES passkeys(id) ON DELETE CASCADE,
  label         TEXT    NOT NULL,      -- client name, for the "signed-in devices" list
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL,
  last_seen_ms  INTEGER NOT NULL
);
```

`ON DELETE CASCADE`: deleting a passkey signs out every session it created. That is the
expected meaning of "remove this device."

> **Deviation from this spec's first draft, recorded rather than made silently:**
> `user_handle` was specified as 32 random bytes and is implemented as a UUID's 16.
> `webauthn-rs` types the user handle as a `Uuid` at its API boundary, so 32 arbitrary
> bytes could not be passed through without a cast that would have to be undone on every
> read. The handle is still opaque, still generated once, still never reused -- only
> narrower.

**In-memory only, never persisted:** the registration and authentication *challenges*.
Same shape and same reasoning as `TicketStore` (`auth.rs`) — a 60-second, single-use
credential must not survive a restart. Reuse that structure rather than inventing a
second one; a restart mid-enrollment just means clicking the button again.

## API surface

New under `/api/v1/auth`. Error codes reuse the existing table
([04](04-api-protocol.md#error-codes)); `unauthorized` covers every failed assertion —
the daemon never distinguishes "no such credential" from "bad signature" in a response
body.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `GET` | `/auth/status` | none | Can this origin do passkeys, and is anything enrolled? |
| `POST` | `/auth/passkey/register/start` | **required** | `PublicKeyCredentialCreationOptions` + challenge id |
| `POST` | `/auth/passkey/register/finish` | **required** | verify attestation, store credential |
| `POST` | `/auth/passkey/login/start` | none | `PublicKeyCredentialRequestOptions` + challenge id |
| `POST` | `/auth/passkey/login/finish` | none | verify assertion → session token |
| `GET` | `/auth/passkeys` | required | list enrolled credentials |
| `PATCH` | `/auth/passkeys/{id}` | required | rename |
| `DELETE` | `/auth/passkeys/{id}` | required | unenroll (cascades sessions) |
| `GET` | `/auth/sessions` | required | list signed-in devices |
| `DELETE` | `/auth/sessions/{id}` | required | revoke one |
| `POST` | `/auth/logout` | required | revoke the caller's own session |

All of these are mutating or credential-bearing, so **every one of them — including the
two unauthenticated ones — goes through `OriginPolicy::check` first.** The `/health`
precedent (unauthenticated, no Origin check) does not extend here: `/health` exists so a
shell can probe before it holds a credential, and it returns nothing an attacker wants.

### `GET /auth/status`

Deliberately the only unauthenticated *informational* endpoint added. It must tell a
freshly-loaded SPA which of three screens to render, before it holds any credential:

```json
{
  "passkey_supported": true,        // this Origin has a usable RP ID
  "rp_id": "localhost",             // null when unsupported
  "enrolled": true,                 // any credential exists for THIS rp_id
  "token_url_hint": "http://localhost:7337"   // present only when passkey_supported is false
}
```

What it leaks: whether a daemon has a passkey enrolled for the origin you can already
reach. Anyone who can reach this endpoint can already reach `/health`. It leaks no
identifier, no `user_name`, and no credential id. Accepted.

### Login response

```json
{ "token": "<64 hex chars>", "expires_at_ms": 1789000000000, "session_id": "01K..." }
```

**A bearer token in the body, not a cookie.** The reasoning, since the cookie is the
more obvious choice:

- The client already has exactly one credential path — `getToken()` →
  `Authorization: Bearer` (`web/src/api/identity.ts`, `web/src/api/api.ts:19`). A session
  token drops into it with no new code path, and `resolve()` in `auth.rs` needs one extra
  branch instead of a parallel cookie-auth implementation.
- A cookie is auto-attached, which creates CSRF surface the token does not have. The
  Origin allowlist would defend it, but "no new attack surface" beats "new attack surface,
  defended."
- The existing WS ticket flow ([06](06-security.md#token-on-the-websocket-upgrade))
  keeps working unchanged: the session token authenticates `POST /ws-ticket` exactly as
  the master token does today.

The cost is XSS-exfiltratability from `localStorage` — but the master token already lives
there, so this is not a regression, and the strict CSP
([06](06-security.md#add-a-strict-content-security-policy)) is the control that matters.

## Flows

### First enrollment (bootstrap)

```text
teleportd prints http://localhost:7337/?token=…      <- token, as today
browser captures + stores the token                  <- captureTokenFromUrl(), unchanged
UI shows "Set up a passkey" (status: supported, not enrolled)
user types a name                                    <- user_name, e.g. "aleh"
POST /auth/passkey/register/start   (Bearer token)   -> creation options, challenge id
navigator.credentials.create(...)                    -> 1Password / Touch ID / key
POST /auth/passkey/register/finish  (Bearer token)   -> owner row + passkeys row written
```

The gate is the bearer token, which only the owning OS user can read (`0600`,
[06](06-security.md#loopback-is-not-a-user-boundary)). **That is what keeps the OS-user
boundary intact:** a second OS user on the host cannot enroll a passkey, because they
cannot read the token, exactly as they cannot call any other API today.

The `owner` row is created by the first enrollment and never again. A second enrollment
reuses the existing `user_handle`, so every credential belongs to the one identity.

### Login

```text
GET /auth/status                     -> { supported: true, enrolled: true }
POST /auth/passkey/login/start       -> request options (allowCredentials for this rp_id), challenge id
navigator.credentials.get(...)       -> 1Password / Touch ID / key
POST /auth/passkey/login/finish      -> { token, expires_at_ms, session_id }
setToken(token); app renders
```

### Second origin (tailnet)

```text
open https://mainpc.tail1234.ts.net/?token=…   (the token, once, as today)
GET /auth/status -> { supported: true, rp_id: "mainpc.tail1234.ts.net", enrolled: false }
UI: "No passkey for this address yet — set one up"     <- not "login failed"
enroll as above; 1Password stores it alongside the localhost one
```

## Principal mapping

```rust
Principal::DeviceToken { token_id: auth_sessions.id }
```

A passkey session is a credential issued to one browser on one device — precisely what
12 describes `DeviceToken` as being
([12](12-identity-and-connectivity.md#the-principal)). The master token continues to
resolve to `LocalUser`. Both authorize identically today; the distinction is recorded so
that stage 3 has something real to build on, and so audit/logging can say *which* device
did a thing.

`resolve()` gains one branch and keeps its shape:

```text
auth_required == false            -> LocalUser                      (unchanged)
presented == master token         -> LocalUser                      (unchanged, constant-time)
presented matches a live session  -> DeviceToken { token_id }       (new)
otherwise                         -> Unauthorized                   (unchanged)
```

Session lookup is by `sha256(presented)` against the `token_sha256` UNIQUE index — an
indexed exact-match on a hash, so no per-request table scan and no timing oracle on the
token value itself.

## Config

```toml
# <data_dir>/config.toml
auth_passkey = true    # default true; false disables every /auth/* route
```

Interactions, stated because they are the easy thing to get wrong:

- `auth_token = false` disables **all** authentication ([06](06-security.md#authentication)).
  Passkeys are meaningless in that mode; `/auth/*` returns `404` and `/auth/status`
  reports `passkey_supported: false`. Do not let one flag half-disable the other.
- `auth_passkey = false` on a daemon with credentials already enrolled: routes are gone,
  existing session tokens stop resolving, the master token still works. That is the
  intended emergency off-switch.
- The RP-ID list is **derived** from `allowed_hosts` + `localhost`. No `passkey_rp_ids`
  setting — a second source of truth for the same fact is exactly what `config.rs`'s
  header warns against.

## Edge cases

| Case | Behavior |
|---|---|
| Every passkey deleted / lost | The master token still works. There is no lockout state, by construction. |
| `<data_dir>/token` regenerated | Passkeys and sessions are unaffected — they key off SQLite, not the token file. |
| Enrolled at `localhost`, visiting `127.0.0.1` | `status.passkey_supported: false`; UI shows the token path and the `localhost` URL to switch to. |
| Browser/authenticator without WebAuthn | Same as above — the token path is always rendered as a fallback, never hidden behind a failed passkey attempt. |
| Non-secure origin (`http://<lan-ip>`) | `passkey_supported: false`. `navigator.credentials` is undefined there; probe it, don't assume. |
| Challenge expired (>60s) | `unauthorized`; the UI restarts the ceremony rather than surfacing a raw error. |
| Challenge replayed | Single-use, removed on any redeem attempt — same rule as `TicketStore::redeem`. |
| Signature counter regresses | Reject the assertion and log a warning. A cloned authenticator is the case this detects. Note: synced providers (1Password, iCloud) legitimately report a constant `0`; a counter that is always `0` is not a regression and must not be treated as one. |
| Daemon restarts mid-ceremony | Challenge is gone; user clicks again. Nothing durable ever depended on it. |
| Session expires while a WS is attached | The WS stays up — it authenticated at upgrade. The next HTTP call gets `401` and the UI returns to login. Sessions keep running regardless; that is invariant 2. |
| Concurrent enrollment from two tabs | Both succeed; two credential rows. Harmless, and matches how every passkey site behaves. |
| `max` enrolled credentials | Capped at 20 per RP ID. Not a security control — a bound so a loop cannot grow the table without limit, same spirit as `max_sessions`. |

## Session lifetime

- **30 days absolute** from issue. No sliding renewal in v1: sliding expiry means every
  request is a write, and a phone that has not been opened in a month re-authenticating
  with a fingerprint is a one-tap cost, not a burden.
- `last_seen_ms` updated at most once per hour per session — enough for a useful "signed-in
  devices" list, cheap enough not to make every GET a database write.
- Expired rows swept by the existing GC task (`spawn_gc_task` in `main.rs`) rather than a
  second timer.

## Dependencies

`webauthn-rs` (0.5), the maintained Rust WebAuthn implementation, plus its transitive
tree. This is a real cost against
[02-stack-decisions.md](02-stack-decisions.md)'s few-deps rule and must be recorded
there.

**It is not negotiable.** The alternative is hand-implementing CBOR parsing, COSE key
decoding, attestation-statement verification, and ES256/RS256 signature checking against
authenticator data — i.e. hand-rolling the cryptographic verification of a credential
that guards a remote-code-execution control plane. The dependency is the conservative
choice here, not the adventurous one.

One `Webauthn` instance is built per RP ID at startup (the crate ties an instance to one
RP ID + origin set), held in `AppState` as a small map. Origins that cannot host a
passkey get no instance, which is also how `/auth/status` answers `passkey_supported`.

## Validation

Per [10-testing.md](10-testing.md).

Unit (`daemon/src/auth.rs`, alongside the existing `resolve`/`TicketStore` tests):

- a valid session token resolves to `DeviceToken` with the right `token_id`
- an expired session token is `Unauthorized`
- a revoked session token is `Unauthorized` on the very next request
- the master token still resolves to `LocalUser` with passkey sessions present
- `auth_token = false` still short-circuits to `LocalUser`
- challenge: single-use, 60s TTL, removed on any redeem attempt (mirrors the ticket tests)
- RP-ID derivation: `localhost` always present; `allowed_hosts` entries included; an IP
  in `allowed_hosts` is excluded, not crashed on

Integration (daemon, with a software authenticator):

- register → login → authenticated request → logout → `401`
- registration with no credential is rejected `401` (the bootstrap gate)
- an assertion for RP `A` presented at RP `B` is rejected
- deleting a passkey immediately invalidates its sessions (the cascade)
- a counter regression is rejected; a constant-zero counter is accepted

Frontend (`web/`, vitest):

- `status.passkey_supported: false` renders the token path, not a broken login
- a `401` mid-session routes back to login without losing the current route
- `identity.ts` prefers a session token over a captured `?token=`

Manual, and not skippable — this is the part unit tests cannot cover:

1. enroll + log in with **1Password** at `localhost`, in Chrome and Safari
2. enroll + log in with platform biometrics (Touch ID) at `localhost`
3. enroll separately at a tailnet hostname; confirm the `localhost` passkey is correctly
   *not* offered there, and that the UI says so rather than erroring
4. confirm the Tauri shell is entirely unaffected
5. delete the last passkey and confirm the token still gets you in

## Doc deltas

Required before implementation, not after:

| Doc | Change |
|---|---|
| [02](02-stack-decisions.md) | Record `webauthn-rs` and why a hand-rolled implementation was refused. |
| [04](04-api-protocol.md) | The `/auth/*` surface above; note that `/auth/status` is unauthenticated but Origin-checked. |
| [05](05-persistence.md) | The three new tables and the new migration index. |
| [06](06-security.md) | Rewrite the closing paragraph of *Authentication*: passwords stay forbidden; passkeys become a second stage-1/2 credential with the token as root of trust. Add the RP-ID/origin matrix. Add three threat-table rows: stolen session token → expiry + revocation; cloned authenticator → counter check; enrollment by another OS user → bootstrap requires the `0600` token. |
| [07](07-remote-access.md) | Per-origin enrollment: adding a tailnet host means enrolling there once. |
| [09](09-frontend.md) | Login screen, settings section, `identity.ts` credential precedence. |
| [11](11-mvp-plan.md) | Leave the out-of-scope line **as-is** — no password system is being built. Add passkey login as a post-M9 item. |
| [12](12-identity-and-connectivity.md) | Stage table: stage 1/2 credential becomes "bearer token **or** passkey session"; `DeviceToken` gains its first real producer. |
| README | Startup URL becomes `localhost`; a line on passkey login. |

## Implementation order

Each step is independently reviewable and leaves the tree working.

1. **Doc deltas** above. Nothing compiles against a doc that still forbids the feature.
2. Startup URL → `localhost` (bind unchanged). Smallest change, unblocks everything, ships alone.
3. Schema migration + `persistence.rs` accessors, with tests. No routes yet.
4. `auth.rs`: session-token branch in `resolve`, challenge store, RP-ID derivation. Tests.
5. `/auth/*` routes + `webauthn-rs` wiring. Integration tests.
6. Web: login screen, settings, `identity.ts` precedence.
7. Manual validation matrix above.
