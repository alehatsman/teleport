<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import Terminal from "./Terminal.svelte";
  import { SessionStream } from "./stream";
  import { setControlling, wasControlling } from "./identity";
  import * as api from "./api";
  import type { Session as SessionData, StreamState } from "./types";

  let { sessionId, onBack }: { sessionId: string; onBack: () => void } = $props();

  let terminalRef: Terminal | undefined = $state();
  let stream: SessionStream | undefined = $state();

  let connectionState: StreamState = $state("connecting");
  let hasControl = $state(false);
  let controllerName: string | null = $state(null);
  let session: SessionData | null = $state(null);
  let toast: string | null = $state(null);
  let truncatedNotice = $state(false);
  let toastTimer: ReturnType<typeof setTimeout> | null = null;

  function showToast(message: string) {
    toast = message;
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => (toast = null), 4000);
  }

  onMount(() => {
    const s = new SessionStream(
      sessionId,
      {
        onState: (state) => (connectionState = state),
        onOutput: (bytes) => terminalRef?.write(bytes),
        onGeometry: (cols, rows) => terminalRef?.setGeometry(cols, rows),
        onControlChange: (has, name) => {
          const wasHolding = hasControl;
          hasControl = has;
          controllerName = name;
          setControlling(sessionId, has);
          if (wasHolding && !has && name) showToast(`Control taken by ${name}`);
        },
        onTruncated: () => {
          terminalRef?.reset();
          truncatedNotice = true;
        },
        onExit: (code) => {
          showToast(code === 0 || code === null ? "Process exited" : `Process exited (code ${code})`);
        },
        onError: (code, message) => {
          if (code === "not_controller") return; // expected when input races a lease change
          showToast(message ?? code);
        },
      },
      // A reopened tab (or a WS drop) resumes control instead of silently
      // dropping to observer -- mode=control never preempts, so this is
      // always safe even if someone else took over in the meantime (the
      // `ready` frame would then just come back control:false).
      { requestControl: wasControlling(sessionId) },
    );
    stream = s;
    s.connect();

    api
      .getSession(sessionId)
      .then((data) => (session = data))
      .catch(() => {
        // Non-fatal -- the header falls back to the raw session id.
      });

    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      document.removeEventListener("visibilitychange", onVisibilityChange);
      if (toastTimer) clearTimeout(toastTimer);
      s.disconnect();
    };
  });

  function onVisibilityChange() {
    // Mobile: the socket is likely dead on resume -- reconnect immediately
    // with the tracked offset instead of waiting for the backoff timer
    // (docs/09-frontend.md#mobile).
    if (document.visibilityState === "visible" && connectionState !== "live" && connectionState !== "connecting") {
      stream?.connect();
    }
  }

  function takeControl() {
    stream?.takeControl();
  }

  function sendKey(bytes: string) {
    if (hasControl) stream?.sendInput(bytes);
  }
</script>

<div class="session">
  <header class="session__header">
    <button class="session__back" onclick={onBack} aria-label="Back to sessions">&larr;</button>
    <h1 class="session__title">{session?.command ?? sessionId}</h1>
    <span class="session__status">
      <span
        class="dot"
        aria-hidden="true"
        class:dot--success={connectionState === "live"}
        class:dot--warning-strong={connectionState === "reconnecting" || connectionState === "connecting"}
        class:dot--pulse={connectionState === "reconnecting" || connectionState === "connecting"}
      ></span>
      <span class="session__status-label">{connectionState}</span>
    </span>
    <span class="session__spacer"></span>
    {#if hasControl}
      <span class="badge badge--controlling">Controlling</span>
    {:else}
      <button class="btn btn--primary" onclick={takeControl}>
        Take control{#if controllerName}&nbsp;(from {controllerName}){/if}
      </button>
    {/if}
  </header>

  {#if truncatedNotice}
    <div class="notice">
      Scrollback truncated.
      <a class="notice__link" href={`/api/v1/sessions/${sessionId}/log`} target="_blank" rel="noreferrer">
        View full log
      </a>
      <button class="notice__dismiss" onclick={() => (truncatedNotice = false)} aria-label="Dismiss">&times;</button>
    </div>
  {/if}

  {#if toast}
    <div class="toast" role="status" aria-live="polite" aria-atomic="true">{toast}</div>
  {/if}

  <main class="session__main" class:session__main--dimmed={!hasControl}>
    {#if stream}
      <Terminal bind:this={terminalRef} {stream} isController={hasControl} />
    {/if}
  </main>

  <div class="key-bar">
    <button class="key-bar__button" onclick={() => sendKey("\x1b")}>Esc</button>
    <button class="key-bar__button" onclick={() => sendKey("\t")}>Tab</button>
    <button class="key-bar__button" onclick={() => sendKey("\x03")}>Ctrl-C</button>
    <button class="key-bar__button" onclick={() => sendKey("\x1b[A")} aria-label="Up">↑</button>
    <button class="key-bar__button" onclick={() => sendKey("\x1b[B")} aria-label="Down">↓</button>
    <button class="key-bar__button" onclick={() => sendKey("\x1b[D")} aria-label="Left">←</button>
    <button class="key-bar__button" onclick={() => sendKey("\x1b[C")} aria-label="Right">→</button>
  </div>
</div>

<style>
  /* Block: session -- one session view (header, terminal, key bar). */
  .session {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  .session__header {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 0.6rem var(--space-3);
    border-bottom: 1px solid var(--border);
    background: var(--surface);
  }
  .session__back {
    background: none;
    border: none;
    color: inherit;
    font-size: 1.1rem;
    cursor: pointer;
    flex-shrink: 0;
    opacity: 0.8;
  }
  .session__back:hover {
    opacity: 1;
  }
  .session__title {
    font-size: 0.95rem;
    font-weight: 600;
    margin: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
  }
  .session__status {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
  }
  .session__status-label {
    font-size: 0.75rem;
    opacity: 0.7;
    text-transform: capitalize;
  }
  .session__spacer {
    flex: 1;
  }
  .session__main {
    flex: 1;
    min-height: 0;
    transition: opacity var(--transition-fast);
  }
  .session__main--dimmed {
    opacity: 0.85;
  }

  /* Block: key-bar -- touch-only row of keys a soft keyboard can't send
     (docs/09-frontend.md#mobile). */
  .key-bar {
    display: none;
    gap: 0.3rem;
    padding: 0.4rem;
    border-top: 1px solid var(--border);
    background: var(--surface);
  }
  .key-bar__button {
    flex: 1;
    background: var(--surface-raised);
    color: inherit;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-md);
    padding: 0.5rem 0;
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
