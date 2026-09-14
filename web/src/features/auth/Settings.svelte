<script lang="ts">
  // The permanent home for credential management (issue #72). Before this,
  // `Passkeys` was rendered only by App.svelte's first-run nudge, which is
  // gated on nothing being enrolled -- so the panel disappeared for good the
  // moment it was used, and four shipped endpoints had no consumer at all.
  import type { AuthStatus } from "@/api/types"
  import AuthSessions from "./AuthSessions.svelte"
  import Passkeys from "./Passkeys.svelte"

  interface Props {
    status: AuthStatus | null
    passkeysUsable: boolean
    onBack: () => void
    onChanged: () => void
    onSignedOut: () => void
  }

  const { status, passkeysUsable, onBack, onChanged, onSignedOut }: Props = $props()
</script>

<div class="settings">
  <header class="settings__header">
    <button class="btn settings__back" type="button" onclick={onBack}>&lsaquo; Sessions</button>
    <h1 class="settings__title">Settings</h1>
  </header>

  <div class="settings__body">
    <Passkeys {status} {passkeysUsable} {onChanged} />
    <AuthSessions {onSignedOut} />
  </div>
</div>

<style>
  .settings {
    display: flex;
    flex-direction: column;
    min-height: 100dvh;
  }

  .settings__header {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    border-bottom: 1px solid var(--border);
  }

  .settings__title {
    margin: 0;
    font-size: 1rem;
    font-weight: 500;
    color: var(--muted);
  }

  .settings__body {
    display: flex;
    flex-direction: column;
    gap: calc(var(--space-4) * 2);
    padding: var(--space-4);
    /* The same column the session list uses, so settings doesn't sprawl to
       the full width of a desktop window. */
    width: 100%;
    max-width: var(--content-max-width);
    margin: 0 auto;
  }
</style>
