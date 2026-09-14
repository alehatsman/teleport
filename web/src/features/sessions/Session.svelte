<script lang="ts">
  import { onMount } from "svelte"
  import { describeError, getSession } from "@/api/api"
  import { setControlling, wasControlling } from "@/api/identity"
  import { SessionStream } from "@/api/stream"
  import { ApiError, type Session as SessionData, type StreamState } from "@/api/types"
  import KeyBar from "@/features/sessions/KeyBar.svelte"
  import SessionHeader from "@/features/sessions/SessionHeader.svelte"
  import Terminal from "@/features/sessions/Terminal.svelte"
  import ErrorBanner from "@/ui/ErrorBanner.svelte"
  import { displayTitle, viewerStatus } from "./sessionDisplay"

  let { sessionId, onBack }: { sessionId: string; onBack: () => void } = $props()

  let terminalRef: Terminal | undefined = $state()
  let stream: SessionStream | undefined = $state()

  let connectionState: StreamState = $state("connecting")
  let hasControl = $state(false)
  let controllerName: string | null = $state(null)
  let session: SessionData | null = $state(null)
  let toast: string | null = $state(null)
  let truncatedNotice = $state(false)
  // Why the session record couldn't be read. A bogus id (a stale link, a
  // purged session) used to render the id as the title, a "Closed" dot and
  // a black canvas -- indistinguishable from a session that simply ended.
  let sessionError: string | null = $state(null)
  // Record first, socket second (sessionDisplay.ts#viewerStatus). $derived.by,
  // not $derived: TS narrows `session` to its `null` initializer at this
  // point in the script (it's only ever reassigned inside callbacks).
  let status = $derived.by(() => viewerStatus(session, connectionState))
  let toastTimer: ReturnType<typeof setTimeout> | null = null
  let controllerPollTimer: ReturnType<typeof setInterval> | null = null
  // Same pragmatic trade-off Sessions.svelte makes for the list (M5, no
  // push channel for this yet): corrects the *displayed* controller name
  // when the client we were told holds the lease disconnects and its own
  // control_grace_ms lapses with no one reconnecting -- freeing the lease
  // server-side generates no frame to tell an idle observer. Never touches
  // `hasControl`: that stays frame-only (docs/09-frontend.md#streamts-the-
  // part-that-must-be-right). A stale name is a display bug; claiming
  // control despite one is not -- claim_control always succeeds.
  const CONTROLLER_POLL_MS = 5000

  function showToast(message: string) {
    toast = message
    if (toastTimer) clearTimeout(toastTimer)
    toastTimer = setTimeout(() => (toast = null), 4000)
  }

  onMount(() => {
    const s = new SessionStream(
      sessionId,
      {
        onState: (state) => {
          connectionState = state
          // `closed` means the daemon has no live entry for this session at
          // all -- a bad id, or (docs/01-architecture.md#the-crash-boundary)
          // one recovered as `lost` after a restart. No more control frames
          // are coming either way, so nothing else will ever clear a stale
          // lease here: reset it now rather than leave "Controlling" over a
          // session with no PTY left, and re-read the record so the header's
          // label comes from `session.state` instead of falling back to the
          // raw connection string (docs/09-frontend.md#control-lease-ui).
          if (state === "closed") {
            clearControl()
            void loadSession()
          }
        },
        onOutput: (bytes) => terminalRef?.write(bytes),
        onGeometry: (cols, rows) => terminalRef?.setGeometry(cols, rows),
        onControlChange: (has, name) => {
          const wasHolding = hasControl
          hasControl = has
          controllerName = name
          setControlling(sessionId, has)
          if (wasHolding && !has && name) showToast(`Control taken by ${name}`)
        },
        onTruncated: () => {
          terminalRef?.reset()
          truncatedNotice = true
        },
        onExit: (code) => {
          // Same fact as the `closed` state above, reached a different way
          // (a live exit frame instead of the daemon losing the session
          // outright): no PTY is left, so no one controls it any more.
          clearControl()
          showToast(
            code === 0 || code === null ? "Process exited" : `Process exited (code ${code})`
          )
          // Re-read the record so the header's verdict outlives the toast.
          void loadSession()
        },
        onError: (code, message) => {
          if (code === "not_controller") return // expected when input races a lease change
          showToast(message ?? code)
        },
      },
      // A reopened tab (or a WS drop) resumes control instead of silently
      // dropping to observer -- mode=control never preempts, so this is
      // always safe even if someone else took over in the meantime (the
      // `ready` frame would then just come back control:false).
      { requestControl: wasControlling(sessionId) }
    )
    stream = s
    s.connect()

    void loadSession()
    controllerPollTimer = setInterval(pollControllerName, CONTROLLER_POLL_MS)

    document.addEventListener("visibilitychange", onVisibilityChange)
    return () => {
      document.removeEventListener("visibilitychange", onVisibilityChange)
      if (toastTimer) clearTimeout(toastTimer)
      if (controllerPollTimer) clearInterval(controllerPollTimer)
      s.disconnect()
    }
  })

  /** A closed connection cannot be controlling anything, whatever the last control frame said. */
  function clearControl() {
    hasControl = false
    controllerName = null
    setControlling(sessionId, false)
  }

  async function loadSession() {
    try {
      session = await getSession(sessionId)
      sessionError = null
    } catch (e) {
      // The header still falls back to the raw id; the banner says why.
      if (e instanceof ApiError && e.status === 404)
        sessionError = "Session not found. It may have been deleted."
      else sessionError = describeError(e)
    }
  }

  // See CONTROLLER_POLL_MS above. Only the displayed name, never `hasControl`.
  async function pollControllerName() {
    if (hasControl) return
    await loadSession()
    if (!hasControl && session) controllerName = session.controller
  }

  function onVisibilityChange() {
    // Mobile: the socket is likely dead on resume -- reconnect immediately
    // with the tracked offset instead of waiting for the backoff timer
    // (docs/09-frontend.md#mobile).
    if (
      document.visibilityState === "visible" &&
      connectionState !== "live" &&
      connectionState !== "connecting"
    ) {
      stream?.connect()
    }
  }

  function takeControl() {
    stream?.takeControl()
  }

  function sendKey(bytes: string) {
    if (hasControl) stream?.sendInput(bytes)
    else onObserverInput()
  }

  const OBSERVER_HINT = "Read-only. Take control to type."
  const ENDED_HINT = "Session ended."

  function onObserverInput() {
    // A session that has already ended shows neither the badge nor the
    // "Take control" button (SessionHeader.svelte) -- telling the user to
    // take control here would send them looking for a button that was
    // deliberately hidden as a lie. Same toast plumbing, different copy.
    const hint = status.ended ? ENDED_HINT : OBSERVER_HINT
    // Repeated keystrokes just keep the same toast alive; don't re-trigger
    // its entrance animation on every key.
    if (toast === hint) {
      if (toastTimer) clearTimeout(toastTimer)
      toastTimer = setTimeout(() => (toast = null), 4000)
      return
    }
    showToast(hint)
  }
</script>

<div class="session">
  <SessionHeader
    title={displayTitle(session, sessionId)}
    tone={status.tone}
    pulse={status.unsettled}
    statusLabel={status.label}
    ended={status.ended}
    {hasControl}
    closed={connectionState === "closed"}
    {controllerName}
    {toast}
    {onBack}
    onTakeControl={takeControl}
  />

  {#if sessionError}
    <div class="session__banner"><ErrorBanner message={sessionError} /></div>
  {/if}

  {#if truncatedNotice}
    <div class="notice">
      Scrollback truncated.
      <a class="notice__link" href={`/api/v1/sessions/${sessionId}/log`} target="_blank" rel="noreferrer">
        View full log
      </a>
      <button class="notice__dismiss" onclick={() => (truncatedNotice = false)} aria-label="Dismiss">&times;</button>
    </div>
  {/if}

  <main class="session__main" class:session__main--dimmed={!hasControl}>
    {#if stream}
      <Terminal bind:this={terminalRef} {stream} isController={hasControl} ended={status.ended} {onObserverInput} />
    {/if}
  </main>

  <KeyBar onKey={sendKey} dimmed={!hasControl} />
</div>

<style>
  /* Block: session -- one session view (header, terminal, key bar). */
  .session {
    display: flex;
    flex-direction: column;
    /* dvh, not vh -- vh includes the area behind mobile Chrome's collapsible
       URL bar, so the page renders taller than what's actually visible
       (docs/09-frontend.md#mobile). Paired with interactive-widget=resizes-
       content in index.html so this also shrinks when the soft keyboard
       opens, instead of leaving the key bar stranded below it. */
    height: 100vh;
    height: 100dvh;
  }
  .session__banner {
    /* .banner's own margin is for stacked page content; here it's a strip
       between header and terminal. Svelte scoping can't reach into
       ErrorBanner's element, so the placement lives on this wrapper. */
    margin: var(--space-2) var(--space-3);
  }
  .session__banner > :global(.banner) {
    margin-bottom: 0;
  }
  .session__main {
    flex: 1;
    min-height: 0;
    transition: opacity var(--transition-fast);
  }
  .session__main--dimmed {
    opacity: 0.85;
  }
</style>
