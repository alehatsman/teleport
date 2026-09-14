// Browser-side WebAuthn plumbing (docs/17-passkey-login.md).
//
// The daemon speaks JSON with base64url strings; `navigator.credentials`
// speaks `ArrayBuffer`. Nothing in the platform converts between the two
// portably yet -- `PublicKeyCredential.parseCreationOptionsFromJSON()` exists
// but is not in every browser this UI has to run in -- so the conversion is
// here, explicitly, in one place.
//
// **This module makes no security decisions.** Every value it touches is
// verified by the daemon against a challenge the daemon issued; mangling
// anything here produces a failed ceremony, not a bypass.

// Returns an `ArrayBuffer` rather than a `Uint8Array` on purpose: the
// WebAuthn dictionaries want `BufferSource` backed by a plain `ArrayBuffer`,
// and a `Uint8Array`'s buffer is typed `ArrayBufferLike` (which admits
// `SharedArrayBuffer`). Allocating the buffer first keeps that honest
// without a cast.
/** Base64url (no padding, `-_` alphabet) to bytes. */
function fromBase64Url(value: string): ArrayBuffer {
  const padded = value.replace(/-/g, "+").replace(/_/g, "/")
  const binary = atob(padded.padEnd(Math.ceil(padded.length / 4) * 4, "="))
  const buffer = new ArrayBuffer(binary.length)
  const bytes = new Uint8Array(buffer)
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i)
  return buffer
}

/** Bytes to base64url, which is the only encoding the daemon accepts back. */
function toBase64Url(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer)
  let binary = ""
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")
}

/**
 * Is a passkey ceremony even possible here? Probed, never inferred from the
 * origin: `/auth/status` already answers the RP-ID half, but a browser
 * without WebAuthn (or a non-secure context, where `navigator.credentials`
 * is undefined) has to be caught client-side
 * (docs/09-frontend.md#credential-precedence-and-the-login-screen).
 */
export function passkeysUsable(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.PublicKeyCredential === "function" &&
    typeof navigator.credentials?.create === "function" &&
    typeof navigator.credentials?.get === "function"
  )
}

// The daemon's options, as they arrive: identical to the WebAuthn dictionaries
// except that every binary field is a base64url string.
interface JsonCredentialDescriptor {
  id: string
  type: string
  transports?: AuthenticatorTransport[]
}

export interface JsonCreationOptions {
  publicKey: {
    rp: { id: string; name: string }
    user: { id: string; name: string; displayName: string }
    challenge: string
    pubKeyCredParams: PublicKeyCredentialParameters[]
    timeout?: number
    excludeCredentials?: JsonCredentialDescriptor[]
    authenticatorSelection?: AuthenticatorSelectionCriteria
    attestation?: AttestationConveyancePreference
    extensions?: Record<string, unknown>
  }
}

export interface JsonRequestOptions {
  publicKey: {
    challenge: string
    timeout?: number
    rpId?: string
    allowCredentials?: JsonCredentialDescriptor[]
    userVerification?: UserVerificationRequirement
    extensions?: Record<string, unknown>
  }
}

// `transports` is omitted rather than set to `undefined`: the project builds
// with `exactOptionalPropertyTypes`, under which those are different things.
function toDescriptors(
  list: JsonCredentialDescriptor[] | undefined
): PublicKeyCredentialDescriptor[] | undefined {
  return list?.map((entry) => ({
    id: fromBase64Url(entry.id),
    type: entry.type as PublicKeyCredentialType,
    ...(entry.transports ? { transports: entry.transports } : {}),
  }))
}

/** Omit-if-absent, which `exactOptionalPropertyTypes` requires. */
function optional<K extends string, V>(key: K, value: V | undefined): Record<string, V> {
  return value === undefined ? {} : { [key]: value }
}

/** Same `exactOptionalPropertyTypes` rule as `toDescriptors`, one level up. */
function withDescriptors<K extends string>(
  key: K,
  list: JsonCredentialDescriptor[] | undefined
): Record<K, PublicKeyCredentialDescriptor[]> | Record<string, never> {
  const converted = toDescriptors(list)
  return converted ? ({ [key]: converted } as Record<K, PublicKeyCredentialDescriptor[]>) : {}
}

/**
 * Runs the enrollment ceremony. Throws whatever the browser throws --
 * including the `NotAllowedError` a user cancel produces, which the caller
 * has to tell apart from a real failure.
 */
export async function createCredential(options: JsonCreationOptions): Promise<unknown> {
  // Fields are listed rather than spread: a blind `...publicKey` would carry
  // the base64url `excludeCredentials` through alongside the converted one,
  // and under `exactOptionalPropertyTypes` that is a type error rather than a
  // silent override.
  const { publicKey } = options
  const credential = (await navigator.credentials.create({
    publicKey: {
      rp: publicKey.rp,
      user: { ...publicKey.user, id: fromBase64Url(publicKey.user.id) },
      challenge: fromBase64Url(publicKey.challenge),
      pubKeyCredParams: publicKey.pubKeyCredParams,
      ...optional("timeout", publicKey.timeout),
      ...optional("attestation", publicKey.attestation),
      ...optional("authenticatorSelection", publicKey.authenticatorSelection),
      ...withDescriptors("excludeCredentials", publicKey.excludeCredentials),
    },
  })) as PublicKeyCredential | null
  if (!credential) throw new Error("the authenticator returned no credential")

  const response = credential.response as AuthenticatorAttestationResponse
  return {
    id: credential.id,
    rawId: toBase64Url(credential.rawId),
    type: credential.type,
    response: {
      attestationObject: toBase64Url(response.attestationObject),
      clientDataJSON: toBase64Url(response.clientDataJSON),
    },
    extensions: {},
  }
}

/** Runs the login ceremony. Same throwing contract as `createCredential`. */
export async function getCredential(options: JsonRequestOptions): Promise<unknown> {
  const { publicKey } = options
  const credential = (await navigator.credentials.get({
    publicKey: {
      challenge: fromBase64Url(publicKey.challenge),
      ...optional("timeout", publicKey.timeout),
      ...optional("rpId", publicKey.rpId),
      ...optional("userVerification", publicKey.userVerification),
      ...withDescriptors("allowCredentials", publicKey.allowCredentials),
    },
  })) as PublicKeyCredential | null
  if (!credential) throw new Error("the authenticator returned no credential")

  const response = credential.response as AuthenticatorAssertionResponse
  return {
    id: credential.id,
    rawId: toBase64Url(credential.rawId),
    type: credential.type,
    response: {
      authenticatorData: toBase64Url(response.authenticatorData),
      clientDataJSON: toBase64Url(response.clientDataJSON),
      signature: toBase64Url(response.signature),
      userHandle: response.userHandle ? toBase64Url(response.userHandle) : null,
    },
    extensions: {},
  }
}

/**
 * Did the user simply cancel? A dismissed Touch ID prompt and a genuinely
 * broken ceremony both arrive as exceptions, and showing "login failed" for
 * the first one is wrong.
 */
export function isUserCancellation(e: unknown): boolean {
  return e instanceof DOMException && (e.name === "NotAllowedError" || e.name === "AbortError")
}

export const __test = { fromBase64Url, toBase64Url }
