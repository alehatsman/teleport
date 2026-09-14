<script lang="ts">
  // The pre-credential screen (docs/09-frontend.md#credential-precedence-and-the-login-screen).
  // Two shapes, never a third: a passkey button when one can actually
  // succeed here, and the token explanation when it cannot. It must never
  // present a button that is guaranteed to fail.
  import { describeError, loginFinish, loginStart } from "@/api/api"
  import { CLIENT_NAME, setSessionToken } from "@/api/identity"
  import type { AuthStatus } from "@/api/types"
  import { getCredential, isUserCancellation } from "@/api/webauthn"
  import ErrorBanner from "@/ui/ErrorBanner.svelte"
  import { unsupportedReason } from "./authDisplay"

  interface Props {
    status: AuthStatus | null
    passkeysUsable: boolean
    canUsePasskey: boolean
    onSignedIn: () => void
  }

  const { status, passkeysUsable, canUsePasskey, onSignedIn }: Props = $props()

  let busy = $state(false)
  let error = $state<string | null>(null)

  async function signIn() {
    busy = true
    error = null
    try {
      const { challenge_id, options } = await loginStart()
      const credential = await getCredential(options)
      const { token } = await loginFinish(challenge_id, credential, CLIENT_NAME)
      setSessionToken(token)
      onSignedIn()
    } catch (e) {
      // A dismissed Touch ID prompt is not a failure worth shouting about;
      // it just means the user changed their mind.
      error = isUserCancellation(e) ? null : describeError(e)
    } finally {
      busy = false
    }
  }
</script>

<main class="login">
  <h1 class="login__title">teleport</h1>

  {#if canUsePasskey}
    <p class="login__lede">Sign in to reach this machine's sessions.</p>
    <button class="btn btn--primary" type="button" onclick={signIn} disabled={busy}>
      {busy ? "Waiting for your passkey…" : "Sign in with a passkey"}
    </button>
  {:else}
    <p class="login__lede">{unsupportedReason(status, passkeysUsable)}</p>
    <p class="login__hint">
      teleportd prints a <code class="login__code">?token=…</code> link when it starts. Open
      that link once and this browser stays signed in.
    </p>
    {#if status?.token_url_hint}
      <a class="btn" href={status.token_url_hint}>Open {status.token_url_hint}</a>
    {/if}
  {/if}

  {#if error}
    <ErrorBanner message={error} />
  {/if}
</main>

<style>
  .login {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    align-items: flex-start;
    max-width: 32rem;
    margin: 0 auto;
    padding: var(--space-4);
    min-height: 100dvh;
    justify-content: center;
  }

  .login__title {
    margin: 0;
    font-size: 1.5rem;
  }

  .login__lede {
    margin: 0;
    color: var(--muted);
  }

  .login__hint {
    margin: 0;
    font-size: 0.875rem;
    color: var(--muted);
  }

  .login__code {
    font-family: var(--font-mono);
  }
</style>
