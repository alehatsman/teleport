<script lang="ts">
  // The viewer's top bar: back, title, status, lease control, and the
  // toast anchored under it. Pure presentation -- Session.svelte owns the
  // stream and decides what each of these means.
  import StatusDot from "@/ui/StatusDot.svelte"
  import type { DotTone } from "@/ui/tones"

  let {
    title,
    tone,
    pulse,
    statusLabel,
    /** Nothing to control any more: neither the badge nor the button renders. */
    ended,
    hasControl,
    /** The socket is gone for good: no lease to ask for. */
    closed,
    controllerName,
    toast,
    onBack,
    onTakeControl,
  }: {
    title: string
    tone: DotTone
    pulse: boolean
    statusLabel: string
    ended: boolean
    hasControl: boolean
    closed: boolean
    controllerName: string | null
    toast: string | null
    onBack: () => void
    onTakeControl: () => void
  } = $props()
</script>

<header class="session-header">
  <div class="session-header__inner">
    <button class="session-header__back" onclick={onBack} aria-label="Back to sessions">&larr;</button>
    <h1 class="session-header__title">{title}</h1>
    <StatusDot {tone} {pulse} label={statusLabel} showLabel />
    <span class="session-header__spacer"></span>
    {#if ended}
      <!-- The badge/button would be a lie either way. -->
    {:else if hasControl}
      <span class="badge badge--controlling">Controlling</span>
    {:else if !closed}
      <button class="btn btn--primary session-header__control-btn" onclick={onTakeControl}>
        Take control{#if controllerName}&nbsp;(from {controllerName}){/if}
      </button>
    {/if}
    {#if toast}
      <div class="toast" role="status" aria-live="polite" aria-atomic="true">{toast}</div>
    {/if}
  </div>
</header>

<style>
  /* Block: session-header -- the one session view's top bar. */
  .session-header {
    /* Full-bleed strip: .session itself is deliberately full-width (the
       terminal below wants it, docs/09-frontend.md#mobile), and this bar's
       background/border-bottom should read as one continuous toolbar
       across the whole screen, not a floating box. Only .session-header__inner
       below caps its *content* width. */
    border-bottom: 1px solid var(--border);
    background: var(--surface);
  }
  .session-header__inner {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    /* Caps the row's content to the same comfortable column .sessions (the
       list view) reads in, instead of "Back" and "Take control" chasing the
       real window edges on a wide desktop -- .session-header above stays a
       full-width strip regardless. */
    max-width: var(--content-max-width);
    margin: 0 auto;
    padding: 0.6rem var(--space-3);
    /* .toast (app.css) is position:absolute; this is its containing block
       -- see the override below. Living on the padded inner row (not the
       outer strip) keeps the toast's height math identical to before this
       split: the strip carries no padding of its own, so its total height
       still equals this row's padded height exactly. */
    position: relative;
  }
  /* .toast is shared (app.css) and normally a `top: 3.25rem` guess at the
     header's height -- wrong by however much the real header differs from
     that guess (font size, safe-area inset, a long controller name),
     eating further into the terminal than intended. Anchored to the header
     itself, it tracks the header's *actual* rendered height exactly instead
     of guessing. It still overlaps the terminal's first line or two for
     its ~4s lifetime -- full-bleed terminal plus a non-reflowing overlay
     leaves nowhere content-free to put it; that part is unchanged. */
  .toast {
    top: 100%;
    margin-top: 0.4rem;
  }
  .session-header__back {
    background: none;
    border: none;
    color: inherit;
    font-size: 1.1rem;
    cursor: pointer;
    flex-shrink: 0;
    opacity: 0.8;
  }
  .session-header__back:hover {
    opacity: 1;
  }
  .session-header__title {
    font-size: 0.95rem;
    font-weight: 600;
    margin: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-family: var(--font-mono);
  }
  .session-header__spacer {
    flex: 1;
  }
  .session-header__control-btn {
    /* Long controller names ("Take control (from Chrome on Linux)") must
       lose to a narrow header gracefully -- plain <button> text wraps by
       default, which on a phone-width header ballooned it to two lines and
       squeezed the title down to a couple of characters. overflow:hidden
       gives this flex item an automatic min-width of 0 so it can shrink
       instead of forcing the wrap; the max-width caps its claim so the
       title keeps a usable share however long the name is (caught live:
       "Take control (from Chrome on macOS)" crushed "claude" to "clau…"). */
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 50%;
  }
</style>
