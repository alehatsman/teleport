<script lang="ts">
  // Touch-only row of keys a soft keyboard can't send
  // (docs/09-frontend.md#mobile). Pure presentation: emits the raw bytes,
  // Session.svelte decides whether this client may send them.
  let {
    onKey,
    /** Mirrors .session__main--dimmed (Session.svelte): true whenever this
        client doesn't hold control, so the one touch input surface doesn't
        look equally tappable as the terminal it contradicts. Not `disabled`
        on the buttons -- a tap must still reach onKey so sendKey's existing
        "Read-only" toast fires; only the terminal's own visual signal is
        being matched here, not its interactivity. */
    dimmed = false,
  }: { onKey: (bytes: string) => void; dimmed?: boolean } = $props()
</script>

<div class="key-bar" class:key-bar--dimmed={dimmed}>
  <div class="key-bar__row">
    <button class="key-bar__button" onclick={() => onKey("\x1b")}>Esc</button>
    <button class="key-bar__button" onclick={() => onKey("\t")}>Tab</button>
    <button class="key-bar__button" onclick={() => onKey("\x03")}>Ctrl-C</button>
    <button class="key-bar__button" onclick={() => onKey("\x1b[Z")}>Shift-Tab</button>
  </div>
  <div class="key-bar__row">
    <button class="key-bar__button" onclick={() => onKey("\x1b[D")} aria-label="Left">←</button>
    <button class="key-bar__button" onclick={() => onKey("\x1b[A")} aria-label="Up">↑</button>
    <button class="key-bar__button" onclick={() => onKey("\x1b[B")} aria-label="Down">↓</button>
    <button class="key-bar__button" onclick={() => onKey("\x1b[C")} aria-label="Right">→</button>
    <button class="key-bar__button" onclick={() => onKey("\r")}>Enter</button>
  </div>
</div>

<style>
  /* Block: key-bar -- hidden on fine-pointer desktops (below). */
  .key-bar {
    display: none;
    flex-direction: column;
    gap: 0.25rem;
    padding: 0.3rem;
    border-top: 1px solid var(--border);
    background: var(--surface);
  }
  .key-bar__row {
    display: flex;
    gap: 0.25rem;
  }
  .key-bar__button {
    flex: 1;
    background: var(--surface-raised);
    color: inherit;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-md);
    padding: 0.3rem 0;
    font-size: 0.8rem;
    /* These are built for a burst of rapid taps (arrows, Ctrl-C). Without
       this, two taps close together anywhere near the same spot are a
       double-tap-to-zoom gesture to the browser first -- the key never
       reaches the PTY and the page zooms instead. */
    touch-action: manipulation;
  }
  .key-bar__button:active {
    background: var(--surface-hover);
  }
  .key-bar--dimmed {
    opacity: 0.85;
  }

  @media (max-width: 700px), (pointer: coarse) {
    .key-bar {
      display: flex;
    }
  }
</style>
