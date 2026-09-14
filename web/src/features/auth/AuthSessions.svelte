<script lang="ts">
  // Signed-in devices, over the `/auth/sessions` routes that shipped with
  // docs/17-passkey-login.md and had no UI consumer (issue #72). This is the
  // only way to revoke a lost device short of deleting the passkey it signed
  // in with, which takes every other device on that credential down too.
  import { onMount } from "svelte"
  import { deleteAuthSession, describeError, listAuthSessions, logout } from "@/api/api"
  import type { AuthSessionSummary } from "@/api/types"
  import ErrorBanner from "@/ui/ErrorBanner.svelte"
  import { describeExpiry, sortAuthSessions } from "./authDisplay"

  interface Props {
    /** Called after the caller's own session is revoked, so the app can
        return to the login screen rather than 401 on its next poll. */
    onSignedOut: () => void
  }

  const { onSignedOut }: Props = $props()

  let sessions = $state<AuthSessionSummary[]>([])
  let busy = $state(false)
  let error = $state<string | null>(null)
  let loaded = $state(false)

  const sorted = $derived(sortAuthSessions(sessions))

  async function refresh() {
    try {
      sessions = await listAuthSessions()
      error = null
    } catch (e) {
      error = describeError(e)
    } finally {
      loaded = true
    }
  }

  onMount(refresh)

  // Revoking your own session is a legitimate thing to want (this device is
  // the one being handed on), but it is not what someone reaching for "sign
  // out a lost phone" means to click, so it confirms and then signs out for
  // real instead of leaving a dead session polling 401s.
  async function revoke(session: AuthSessionSummary) {
    if (session.current && !confirm("Revoke this device? You'll be signed out here.")) return
    busy = true
    error = null
    try {
      await deleteAuthSession(session.id)
      if (session.current) {
        onSignedOut()
        return
      }
      await refresh()
    } catch (e) {
      error = describeError(e)
    } finally {
      busy = false
    }
  }

  async function signOut() {
    busy = true
    error = null
    try {
      await logout()
      onSignedOut()
    } catch (e) {
      error = describeError(e)
      busy = false
    }
  }
</script>

<section class="devices">
  <h2 class="devices__title">Signed-in devices</h2>

  {#if loaded && sorted.length === 0}
    <p class="devices__empty">
      No passkey sessions. You're signed in with the startup token, which has no session
      to revoke.
    </p>
  {/if}

  <ul class="devices__list">
    {#each sorted as session (session.id)}
      <li class="devices__item" class:devices__item--current={session.current}>
        <span class="devices__label">
          {session.label}
          {#if session.current}<span class="devices__badge">this device</span>{/if}
        </span>
        <span class="devices__meta">{describeExpiry(session.expires_at_ms, Date.now())}</span>
        <button
          class="btn btn--danger"
          type="button"
          onclick={() => revoke(session)}
          disabled={busy}
        >
          Revoke
        </button>
      </li>
    {/each}
  </ul>

  {#if sorted.some((s) => s.current)}
    <button class="btn" type="button" onclick={signOut} disabled={busy}>Sign out</button>
  {/if}

  {#if error}
    <ErrorBanner message={error} />
  {/if}
</section>

<style>
  .devices {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-3);
  }

  .devices__title {
    margin: 0;
    font-size: 1.125rem;
  }

  .devices__empty {
    margin: 0;
    color: var(--muted);
    font-size: 0.875rem;
  }

  .devices__list {
    list-style: none;
    margin: 0;
    padding: 0;
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .devices__item {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2);
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
  }

  .devices__item--current {
    border-color: var(--accent);
  }

  .devices__label {
    flex: 1 1 auto;
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .devices__badge {
    padding: 0 var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    color: var(--muted);
    font-size: 0.75rem;
  }

  .devices__meta {
    color: var(--muted);
    font-size: 0.8125rem;
  }
</style>
