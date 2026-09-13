<script lang="ts">
  // Touch-only row of keys a soft keyboard can't send
  // (docs/09-frontend.md#mobile). Pure presentation: emits the raw bytes,
  // Session.svelte decides whether this client may send them.
  let { onKey }: { onKey: (bytes: string) => void } = $props()
</script>

<div class="key-bar">
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

  @media (max-width: 700px), (pointer: coarse) {
    .key-bar {
      display: flex;
    }
  }
</style>
