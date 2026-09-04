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

<div class="session-view">
  <header>
    <button class="back" onclick={onBack} aria-label="Back to sessions">&larr;</button>
    <h1 class="title">{session?.command ?? sessionId}</h1>
    <span
      class="status-dot"
      aria-hidden="true"
      class:live={connectionState === "live"}
      class:reconnecting={connectionState === "reconnecting" || connectionState === "connecting"}
    ></span>
    <span class="status-label">{connectionState}</span>
    <span class="spacer"></span>
    {#if hasControl}
      <span class="badge controlling">Controlling</span>
    {:else}
      <button class="take-control" onclick={takeControl}>
        Take control{#if controllerName}&nbsp;(from {controllerName}){/if}
      </button>
    {/if}
  </header>

  {#if truncatedNotice}
    <div class="notice">
      Scrollback truncated.
      <a href={`/api/v1/sessions/${sessionId}/log`} target="_blank" rel="noreferrer">View full log</a>
      <button class="dismiss" onclick={() => (truncatedNotice = false)} aria-label="Dismiss">&times;</button>
    </div>
  {/if}

  {#if toast}
    <div class="toast" role="status" aria-live="polite" aria-atomic="true">{toast}</div>
  {/if}

  <main class="terminal-area" class:dimmed={!hasControl}>
    {#if stream}
      <Terminal bind:this={terminalRef} {stream} isController={hasControl} />
    {/if}
  </main>

  <div class="key-bar">
    <button onclick={() => sendKey("\x1b")}>Esc</button>
    <button onclick={() => sendKey("\t")}>Tab</button>
    <button onclick={() => sendKey("\x03")}>Ctrl-C</button>
    <button onclick={() => sendKey("\x1b[A")} aria-label="Up">↑</button>
    <button onclick={() => sendKey("\x1b[B")} aria-label="Down">↓</button>
    <button onclick={() => sendKey("\x1b[D")} aria-label="Left">←</button>
    <button onclick={() => sendKey("\x1b[C")} aria-label="Right">→</button>
  </div>
</div>

<style>
  .session-view {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    border-bottom: 1px solid var(--border);
  }
  .title {
    font-size: inherit;
    font-weight: 600;
    margin: 0;
  }
  .status-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--muted);
  }
  .status-dot.live {
    background: var(--success);
  }
  .status-dot.reconnecting {
    background: var(--warning-strong);
  }
  .status-label {
    font-size: 0.75rem;
    opacity: 0.7;
    text-transform: capitalize;
  }
  .spacer {
    flex: 1;
  }
  .badge.controlling {
    font-size: 0.75rem;
    background: var(--badge-bg);
    color: var(--badge-fg);
    padding: 0.2rem 0.5rem;
    border-radius: 4px;
  }
  .take-control {
    background: var(--accent);
    color: var(--accent-fg);
    border: none;
    border-radius: 4px;
    padding: 0.3rem 0.6rem;
    cursor: pointer;
  }
  .notice {
    background: var(--notice-bg);
    color: var(--notice-fg);
    padding: 0.4rem 0.75rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.85rem;
  }
  .notice a {
    color: inherit;
  }
  .notice .dismiss {
    margin-left: auto;
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
  }
  .toast {
    position: absolute;
    top: 3rem;
    right: 1rem;
    background: var(--surface-raised);
    border: 1px solid var(--border-strong);
    padding: 0.5rem 0.75rem;
    border-radius: 6px;
    font-size: 0.85rem;
    z-index: 10;
  }
  .terminal-area {
    flex: 1;
    min-height: 0;
  }
  .terminal-area.dimmed {
    opacity: 0.85;
  }
  .key-bar {
    display: none;
    gap: 0.25rem;
    padding: 0.4rem;
    border-top: 1px solid var(--border);
  }
  .key-bar button {
    flex: 1;
    background: var(--surface-raised);
    color: inherit;
    border: 1px solid var(--border-strong);
    border-radius: 4px;
    padding: 0.5rem 0;
  }
  .back {
    background: none;
    border: none;
    color: inherit;
    font-size: 1.1rem;
    cursor: pointer;
  }

  /* The key bar exists for what a soft keyboard can't send
     (docs/09-frontend.md#mobile) -- desktop already has these keys. */
  @media (max-width: 700px), (pointer: coarse) {
    .key-bar {
      display: flex;
    }
  }
</style>
