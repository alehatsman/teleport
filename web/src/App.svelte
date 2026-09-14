<script lang="ts">
  // Routing between list and session views (docs/09-frontend.md#structure).
  // No router library -- a hash suffices for two view types and keeps
  // "no state-management library" (docs/09-frontend.md#explicitly-not-in-the-frontend).
  import { onDestroy, onMount } from "svelte"
  import { authStatus } from "@/api/api"
  import { hasAnyToken } from "@/api/identity"
  import type { AuthStatus } from "@/api/types"
  import { passkeysUsable } from "@/api/webauthn"
  import { chooseScreen, shouldOfferSetup } from "@/features/auth/authDisplay"
  import Login from "@/features/auth/Login.svelte"
  import Passkeys from "@/features/auth/Passkeys.svelte"
  import Settings from "@/features/auth/Settings.svelte"
  import Session from "@/features/sessions/Session.svelte"
  import Sessions from "@/features/sessions/Sessions.svelte"

  let hash = $state(window.location.hash)

  function onHashChange() {
    hash = window.location.hash
  }

  onMount(() => window.addEventListener("hashchange", onHashChange))
  onDestroy(() => window.removeEventListener("hashchange", onHashChange))

  let sessionId = $derived(hash.match(/^#\/sessions\/(.+)$/)?.[1] ?? null)
  // A route, not a modal: settings is somewhere you navigate to, so the back
  // button and a shared link both work, same as #/sessions/<id> (issue #72).
  let showSettings = $derived(hash === "#/settings")

  function openSession(id: string) {
    window.location.hash = `#/sessions/${id}`
  }

  function backToList() {
    window.location.hash = "#/"
  }

  function openSettings() {
    window.location.hash = "#/settings"
  }

  // --- Auth gate (docs/17-passkey-login.md) ---

  // Probed once, not derived from the origin: a browser without WebAuthn on
  // a perfectly good origin still cannot run a ceremony.
  const usable = passkeysUsable()
  let status = $state<AuthStatus | null>(null)
  let credential = $state(hasAnyToken())

  // Unauthenticated on purpose -- this is what tells a credential-less SPA
  // which screen to draw. A failure is not fatal: `chooseScreen` treats a
  // null status as "fall back to the token path", which is always correct.
  async function loadStatus() {
    try {
      status = await authStatus()
    } catch {
      status = null
    }
  }

  onMount(loadStatus)

  const screen = $derived(chooseScreen(status, credential, usable))
  const offerSetup = $derived(shouldOfferSetup(status, credential, usable))
  let setupOpen = $state(false)

  function onSignedIn() {
    credential = true
    void loadStatus()
  }

  // Sign-out drops the passkey session; the token, if one was ever pasted,
  // is a separate credential and `hasAnyToken` re-reads it. Re-deriving from
  // storage rather than assuming `false` keeps a token holder in the app
  // instead of bouncing them to a login screen they do not need.
  function onSignedOut() {
    credential = hasAnyToken()
    backToList()
    void loadStatus()
  }
</script>

{#if screen !== "app"}
  <Login {status} passkeysUsable={usable} canUsePasskey={screen === "passkey-login"} {onSignedIn} />
{:else if showSettings}
  <Settings
    {status}
    passkeysUsable={usable}
    onBack={backToList}
    onChanged={loadStatus}
    {onSignedOut}
  />
{:else if sessionId}
  <!-- Keyed on sessionId: the hash can go straight from one #/sessions/X to
       another #/sessions/Y without passing back through "#/" (a shared link,
       a bookmark, Safari's own back/forward across two session views) --
       without the key Svelte reuses this component across that change,
       onMount never re-fires, and the old session's stream/data stays on
       screen mislabeled as the new one. -->
  {#key sessionId}
    <Session sessionId={sessionId} onBack={backToList} />
  {/key}
{:else}
  {#if offerSetup}
    <div class="app-setup">
      {#if setupOpen}
        <Passkeys {status} passkeysUsable={usable} onChanged={loadStatus} />
      {:else}
        <p class="app-setup__lede">
          Signed in with the startup token. Set up a passkey to skip it next time.
        </p>
        <button class="btn" type="button" onclick={() => (setupOpen = true)}>
          Set up a passkey
        </button>
      {/if}
    </div>
  {/if}
  <Sessions onOpen={openSession} {openSettings} />
{/if}

<style>
  .app-setup {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-4);
    border-bottom: 1px solid var(--border);
  }

  .app-setup__lede {
    margin: 0;
    color: var(--muted);
    font-size: 0.875rem;
  }
</style>
