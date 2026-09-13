<script lang="ts">
  // Fixed, thumb-reachable twin of the header's "New session" button --
  // same action, just easier to hit one-handed on a phone. Hidden on a
  // fine-pointer device (below): with a mouse the header button is one
  // short move away and this was a third "New session" on an empty screen.
  // Same input-type split SessionRow's swipe action uses.
  let { expanded, onclick }: { expanded: boolean; onclick: () => void } = $props()
</script>

<button
  class="fab"
  {onclick}
  aria-expanded={expanded}
  aria-controls="launcher-panel"
  aria-label="New session"
>
  <svg class="fab__icon" viewBox="0 0 24 24" aria-hidden="true">
    <path d="M12 5v14M5 12h14" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" fill="none" />
  </svg>
</button>

<style>
  /* Block: fab -- round, gradient + shadow borrowed straight from
     .btn--primary / --shadow-panel rather than inventing a second visual
     language for "primary action". */
  .fab {
    position: fixed;
    right: var(--space-4);
    /* env() falls back to 0 with no viewport-fit=cover meta, same as not
       being there at all -- safe to always include. */
    bottom: calc(var(--space-4) + env(safe-area-inset-bottom, 0px));
    width: 56px;
    height: 56px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    border: none;
    cursor: pointer;
    background: linear-gradient(180deg, var(--accent-hover), var(--accent));
    color: var(--accent-fg);
    box-shadow: var(--shadow-panel);
    z-index: 5; /* above list content, below .toast's 10 */
    transition:
      transform var(--transition-fast),
      box-shadow var(--transition-fast);
  }
  .fab:hover {
    box-shadow: var(--shadow-glow);
    transform: translateY(-2px);
  }
  .fab:active {
    transform: translateY(0) scale(0.94);
  }
  .fab__icon {
    width: 26px;
    height: 26px;
  }
  @media (hover: hover) and (pointer: fine) {
    .fab {
      display: none;
    }
  }
</style>
