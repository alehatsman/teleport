<script lang="ts">
  // Enrollment and management (docs/17-passkey-login.md). Enrollment is
  // gated server-side on an existing credential, so this panel is only ever
  // reachable by someone already signed in -- there is no unauthenticated
  // path to it and none should be added.
  import { onMount } from "svelte"
  import {
    deletePasskey,
    describeError,
    listPasskeys,
    registerFinish,
    registerStart,
  } from "@/api/api"
  import type { AuthStatus, PasskeySummary } from "@/api/types"
  import { createCredential, isUserCancellation } from "@/api/webauthn"
  import ErrorBanner from "@/ui/ErrorBanner.svelte"
  import { describeLastUsed, groupByRp } from "./authDisplay"

  interface Props {
    status: AuthStatus | null
    passkeysUsable: boolean
    onChanged?: () => void
  }

  const { status, passkeysUsable, onChanged }: Props = $props()

  let passkeys = $state<PasskeySummary[]>([])
  let busy = $state(false)
  let error = $state<string | null>(null)
  let label = $state("")

  const canEnroll = $derived(Boolean(passkeysUsable && status?.passkey_supported))
  const grouped = $derived(groupByRp(passkeys))

  async function refresh() {
    try {
      passkeys = await listPasskeys()
      error = null
    } catch (e) {
      error = describeError(e)
    }
  }

  onMount(refresh)

  async function enroll() {
    busy = true
    error = null
    try {
      const { challenge_id, options } = await registerStart(label.trim() || undefined)
      const credential = await createCredential(options)
      await registerFinish(challenge_id, credential, label.trim() || undefined)
      label = ""
      await refresh()
      onChanged?.()
    } catch (e) {
      error = isUserCancellation(e) ? null : describeError(e)
    } finally {
      busy = false
    }
  }

  async function remove(id: string) {
    busy = true
    error = null
    try {
      await deletePasskey(id)
      await refresh()
      onChanged?.()
    } catch (e) {
      error = describeError(e)
    } finally {
      busy = false
    }
  }
</script>

<section class="passkeys">
  <h2 class="passkeys__title">Passkeys</h2>

  {#if canEnroll}
    <p class="passkeys__lede">
      Enrolled for <code class="passkeys__rp">{status?.rp_id}</code>. A passkey works only on
      the address it was created for — enroll again on each address you use.
    </p>
    <div class="passkeys__add">
      <input
        class="passkeys__input"
        type="text"
        bind:value={label}
        placeholder="Name this passkey"
        disabled={busy}
      />
      <button class="btn btn--primary" type="button" onclick={enroll} disabled={busy}>
        {busy ? "Waiting…" : "Add passkey"}
      </button>
    </div>
  {:else}
    <p class="passkeys__lede">
      Passkeys can't be added from this address. Open the <code class="passkeys__rp"
        >localhost</code
      > URL teleportd printed at startup and add one there.
    </p>
  {/if}

  {#each grouped as [rpId, group] (rpId)}
    <h3 class="passkeys__group">{rpId}</h3>
    <ul class="passkeys__list">
      {#each group as passkey (passkey.id)}
        <li class="passkeys__item">
          <span class="passkeys__label">{passkey.label}</span>
          <span class="passkeys__meta">{describeLastUsed(passkey.last_used_ms, Date.now())}</span>
          <button
            class="btn btn--danger"
            type="button"
            onclick={() => remove(passkey.id)}
            disabled={busy}
          >
            Remove
          </button>
        </li>
      {/each}
    </ul>
  {:else}
    <p class="passkeys__empty">No passkeys yet.</p>
  {/each}

  <p class="passkeys__footnote">
    Removing every passkey is safe: the startup token still signs you in.
  </p>

  {#if error}
    <ErrorBanner message={error} />
  {/if}
</section>

<style>
  .passkeys {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .passkeys__title {
    margin: 0;
    font-size: 1.125rem;
  }

  .passkeys__lede,
  .passkeys__empty,
  .passkeys__footnote {
    margin: 0;
    color: var(--muted);
    font-size: 0.875rem;
  }

  .passkeys__rp {
    font-family: var(--font-mono);
  }

  .passkeys__add {
    display: flex;
    gap: var(--space-2);
    flex-wrap: wrap;
  }

  .passkeys__input {
    flex: 1 1 12rem;
    padding: var(--space-2);
    background: var(--surface);
    color: var(--fg);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
  }

  .passkeys__group {
    margin: 0;
    font-family: var(--font-mono);
    font-size: 0.8125rem;
    color: var(--muted);
  }

  .passkeys__list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .passkeys__item {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2);
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
  }

  .passkeys__label {
    flex: 1 1 auto;
  }

  .passkeys__meta {
    color: var(--muted);
    font-size: 0.8125rem;
  }
</style>
