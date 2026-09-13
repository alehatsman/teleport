<script lang="ts">
  import type { Session } from "@/api/types"
  import StatusDot from "@/ui/StatusDot.svelte"
  import {
    displayAge,
    displayCwd,
    displayOutcome,
    needsAttention,
    outcomeFailed,
    STATE_LABELS,
    stateTone,
  } from "./sessionDisplay"

  // -- iOS-style swipe-to-reveal ------------------------------------
  //
  // The action button (terminate/delete) is a real element in the DOM at
  // all times, not conjured up by the gesture -- a mouse has no touch
  // events to swipe with at all, so on a device with a fine pointer
  // (`@media (hover: hover) and (pointer: fine)` below) the row leaves
  // permanent room for it and it's just... a button, always visible, no
  // gesture required. Touch devices additionally get the swipe: the front
  // layer covers the action button by default (plain DOM paint order, no
  // z-index needed) and dragging it left slides it out of the way.
  //
  // Only one row is open at a time -- SessionList owns that exclusivity via
  // `isOpen`/`onOpenChange`; this component only knows about its own row.
  // REVEAL_PX must match .session-row__action's width below (kept as plain
  // numbers, not a shared CSS custom property -- it's one value, every use
  // next to a comment pointing at the other).
  const REVEAL_PX = 72
  const OPEN_THRESHOLD_PX = REVEAL_PX / 2

  let {
    session,
    now,
    homeDir,
    isOpen,
    onOpenChange,
    onOpen,
    onResume,
    onTerminate,
    onPurge,
  }: {
    session: Session
    now: number
    homeDir: string | null
    isOpen: boolean
    onOpenChange: (open: boolean) => void
    onOpen: (id: string) => void
    onResume: (session: Session) => void
    onTerminate: (id: string) => void
    onPurge: (id: string) => void
  } = $props()

  let isDeletable = $derived(session.state === "exited" || session.state === "lost")

  let dragging = $state(false)
  let dragOffsetPx = $state(0)
  let touchStartX = 0
  let touchStartY = 0
  let touchDirection: "horizontal" | "vertical" | null = null

  /** The live transform for this row's front layer -- mid-drag, snapped open, or resting closed. */
  let frontOffsetPx = $derived(dragging ? dragOffsetPx : isOpen ? -REVEAL_PX : 0)

  function onTouchStart(e: TouchEvent) {
    const touch = e.touches[0]
    if (!touch) return
    touchStartX = touch.clientX
    touchStartY = touch.clientY
    touchDirection = null
    dragging = true
    // Start from wherever this row already sits -- swiping an open row
    // shut feels continuous instead of jumping back to 0 first.
    dragOffsetPx = isOpen ? -REVEAL_PX : 0
  }

  function onTouchMove(e: TouchEvent) {
    if (!dragging) return
    const touch = e.touches[0]
    if (!touch) return
    const dx = touch.clientX - touchStartX
    const dy = touch.clientY - touchStartY
    if (touchDirection === null) {
      // A few px of wobble right at touchdown is normal on any gesture --
      // don't commit to horizontal (swipe) vs vertical (scroll) before the
      // direction is actually clear.
      if (Math.abs(dx) < 8 && Math.abs(dy) < 8) return
      touchDirection = Math.abs(dx) > Math.abs(dy) ? "horizontal" : "vertical"
      if (touchDirection === "vertical") {
        // This is a page scroll, not a swipe -- let go and let the browser's
        // own native scrolling handle it from here (touch-action: pan-y
        // below keeps that native path unblocked while we're undecided).
        dragging = false
        return
      }
    }
    if (touchDirection !== "horizontal") return
    e.preventDefault() // committed to a horizontal swipe now -- stop the page scrolling along with it
    const base = isOpen ? -REVEAL_PX : 0
    dragOffsetPx = Math.min(0, Math.max(-REVEAL_PX, base + dx))
  }

  function onTouchEnd() {
    if (!dragging) return
    dragging = false
    onOpenChange(dragOffsetPx <= -OPEN_THRESHOLD_PX)
  }

  function handleAction() {
    onOpenChange(false)
    if (isDeletable) onPurge(session.id)
    else onTerminate(session.id)
  }
</script>

<li class="session-row">
  <!-- svelte-ignore a11y_no_static_element_interactions -- a pure swipe-gesture
       surface, not itself a control: the actual interactive elements are the <a>
       link inside it and the .session-row__action button beside it, both fully
       operable by keyboard/AT with no dependency on these touch handlers. -->
  <div
    class="session-row__front"
    style="transform: translateX({frontOffsetPx}px)"
    ontouchstart={onTouchStart}
    ontouchmove={onTouchMove}
    ontouchend={onTouchEnd}
    ontouchcancel={onTouchEnd}
  >
    <a
      class="session-row__link"
      href={`#/sessions/${session.id}`}
      onclick={(e) => {
        if (isOpen) {
          // Swiped open -- the tap dismisses the reveal instead of also
          // navigating, same as tapping the content of an open iOS swipe
          // action does.
          e.preventDefault();
          onOpenChange(false);
          return;
        }
        onOpen(session.id);
      }}
    >
      <StatusDot tone={stateTone(session.state)} label={STATE_LABELS[session.state]} />
      {#if needsAttention(session, now)}
        <span class="session-row__attention" aria-hidden="true">●</span>
        <span class="sr-only">Needs attention.</span>
      {/if}
      <span class="session-row__command">{session.command}</span>
      {#if session.args.length > 0}
        <!-- The command alone is "sh" or "claude" on every row; the
             args are what made this launch this launch. -->
        <span class="session-row__args">{session.args.join(" ")}</span>
      {/if}
      {#if session.title}
        <span class="session-row__title">({session.title})</span>
      {/if}
      <!-- <bdi> keeps the path itself left-to-right inside the
           rtl-ellipsis trick on .session-row__cwd below. -->
      <span class="session-row__cwd"><bdi>{displayCwd(session.cwd, homeDir)}</bdi></span>
      {#if session.controller}
        <span class="session-row__controller">controlled by {session.controller}</span>
      {/if}
      {#if displayOutcome(session)}
        <span class="session-row__outcome" class:session-row__outcome--failed={outcomeFailed(session)}>{displayOutcome(session)}</span>
      {/if}
      <span class="session-row__age" title={new Date(session.created_at_ms).toLocaleString()}>{displayAge(session.created_at_ms, now)}</span>
    </a>
    {#if isDeletable && session.claude_resume_id}
      <button type="button" class="session-row__resume" onclick={() => onResume(session)}>
        ↻ Resume
      </button>
    {/if}
  </div>
  <button
    class="session-row__action"
    class:session-row__action--danger={isDeletable}
    aria-label={isDeletable ? "Delete session" : "Terminate session"}
    onclick={handleAction}
  >
    <svg class="session-row__action-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path d="M6 6l12 12M18 6L6 18" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" fill="none" />
    </svg>
  </button>
</li>

<style>
  /* Block: session-row -- one row in the session list. Two layers: a
     trailing action button that's always in the DOM (position: absolute,
     first in source order) and a front layer on top of it at full width by
     default -- plain paint order hides the action with zero z-index rules,
     no JS needed to "reveal" it, just a transform to slide the front layer
     out of the way (touch) or narrow it to leave room (pointer:fine below). */
  .session-row {
    position: relative;
    overflow: hidden; /* clips the front layer's slide + the action button to this row's own rounded corners */
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    transition:
      border-color var(--transition-fast),
      box-shadow var(--transition-fast);
  }
  .session-row:hover {
    border-color: var(--border-strong);
    box-shadow: var(--shadow-raised);
  }
  /* REVEAL_PX in the script block must match this width. */
  .session-row__action {
    position: absolute;
    inset: 0 0 0 auto;
    width: 72px;
    /* .session-row__link needs to come before this button in DOM order for
       correct tab order (content before the destructive action), but that
       makes this button the *later* of the two positioned siblings --
       painted on top by default under plain DOM-order stacking, which is
       backwards from what a hidden-until-swiped action needs. Explicit
       z-index (below), not source order, decides paint order here. */
    z-index: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    border: none;
    cursor: pointer;
    color: var(--fg);
    background: var(--surface-hover);
  }
  .session-row__action--danger {
    background: var(--danger-bg);
    color: var(--danger-fg);
  }
  .session-row__action-icon {
    width: 18px;
    height: 18px;
  }
  .session-row__front {
    position: relative; /* enters the same explicit stacking order as .session-row__action -- see its z-index comment */
    z-index: 1;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    background: var(--surface);
    padding: 0.7rem var(--space-3);
    /* Touch target floor. The text alone settles at ~41px. */
    min-height: 44px;
    transition: transform var(--transition-fast);
    /* Let the browser's native scroller own vertical panning; our touch
       handlers only ever act on a horizontal drag (and preventDefault()
       there once it's clearly one), so this keeps page scroll from
       stuttering while a gesture is still ambiguous. */
    touch-action: pan-y;
  }
  /* A mouse has no swipe to reveal the action with -- leave it permanently
     visible instead of hiding a destructive action behind a gesture that
     doesn't exist on this input type (matches .key-bar's own
     touch-only/pointer-fine split in Session.svelte). */
  @media (hover: hover) and (pointer: fine) {
    .session-row__front {
      width: calc(100% - 72px);
    }
  }
  .session-row__link {
    flex: 1;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: none;
    border: none;
    text-align: left;
    text-decoration: none;
    color: inherit;
    cursor: pointer;
    padding: 0;
    overflow: hidden;
    min-width: 0;
  }
  .session-row__attention {
    color: var(--attention);
    font-size: 0.7rem;
    flex-shrink: 0;
  }
  .session-row__command {
    font-weight: 600;
    font-family: var(--font-mono);
    font-size: 0.9rem;
  }
  .session-row__args {
    opacity: 0.8;
    font-size: 0.85rem;
    font-family: var(--font-mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
    /* Args and cwd both ellipsize; args carry more identity, so they keep
       their natural width up to a cap and cwd absorbs the rest of the
       squeeze. (A shrink *ratio* alone didn't do it -- cwd's larger basis
       still left args crushed to "-c sleep 1…" next to a long path.) */
    flex-shrink: 0;
    max-width: 45%;
  }
  .session-row__age {
    margin-left: auto;
    flex-shrink: 0;
    opacity: 0.55;
    font-size: 0.75rem;
    font-variant-numeric: tabular-nums;
  }
  .session-row__outcome {
    margin-left: auto;
    flex-shrink: 0;
    font-size: 0.75rem;
    font-family: var(--font-mono);
    opacity: 0.7;
  }
  .session-row__outcome--failed {
    color: var(--danger-fg);
    opacity: 0.9;
  }
  /* Both right-aligned; when the outcome is present the age sits after it
     without a second auto margin pushing them apart. */
  .session-row__outcome + .session-row__age {
    margin-left: 0;
  }
  .session-row__cwd {
    opacity: 0.55;
    font-size: 0.8rem;
    font-family: var(--font-mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    /* Ellipsize the *start*: "~/projects/teleport/.dev-data/a-very-long…"
       lost the leaf directory, the only part that told rows apart. Laying
       the box out rtl puts the ellipsis on the left; the <bdi> inside
       (isolated, forced ltr) keeps the path reading normally. */
    direction: rtl;
    text-align: left;
  }
  .session-row__cwd > bdi {
    direction: ltr;
    unicode-bidi: isolate;
  }
  .session-row__title {
    /* The agent's own words, not teleport's -- distinct from .session-row__command
       (what's running) and .session-row__cwd (where), so it reads as neither. */
    font-style: italic;
    opacity: 0.7;
    font-size: 0.85rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .session-row__resume {
    flex-shrink: 0;
    margin-left: auto;
    padding: 0.3rem 0.6rem;
    font-size: 0.8rem;
    color: var(--accent);
    background: none;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    cursor: pointer;
  }
  .session-row__resume:hover {
    background: var(--surface-hover);
  }
  .session-row__controller {
    margin-left: auto;
    font-size: 0.75rem;
    opacity: 0.7;
    /* Long client names ("controlled by Chrome on Linux") must lose to the
       narrow viewport gracefully -- flex-shrink:0 let this get hard-clipped
       by .session-row__link's overflow:hidden with no ellipsis on mobile.
       min-width:0 is required for a flex item to actually shrink past its
       content size. */
    flex-shrink: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
