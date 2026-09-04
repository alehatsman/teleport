<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import Terminal from "./Terminal.svelte";
  import { SessionStream } from "./stream";
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
    const s = new SessionStream(sessionId, {
      onState: (state) => (connectionState = state),
      onOutput: (bytes) => terminalRef?.write(bytes),
      onGeometry: (cols, rows) => terminalRef?.setGeometry(cols, rows),
      onControlChange: (has, name) => {
        const wasControlling = hasControl;
        hasControl = has;
        controllerName = name;
        if (wasControlling && !has && name) showToast(`Control taken by ${name}`);
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
    });
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
    <button class="back" onclick={onBack}>&larr;</button>
    <span class="title">{session?.command ?? sessionId}</span>
    <span
      class="status-dot"
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
      <button class="dismiss" onclick={() => (truncatedNotice = false)}>&times;</button>
    </div>
  {/if}

  {#if toast}
    <div class="toast">{toast}</div>
  {/if}

  <div class="terminal-area" class:dimmed={!hasControl}>
    {#if stream}
      <Terminal bind:this={terminalRef} {stream} isController={hasControl} />
    {/if}
  </div>

  <div class="key-bar">
    <button onclick={() => sendKey("\x1b")}>Esc</button>
    <button onclick={() => sendKey("\t")}>Tab</button>
    <button onclick={() => sendKey("\x03")}>Ctrl-C</button>
    <button onclick={() => sendKey("\x1b[A")}>↑</button>
    <button onclick={() => sendKey("\x1b[B")}>↓</button>
    <button onclick={() => sendKey("\x1b[D")}>←</button>
    <button onclick={() => sendKey("\x1b[C")}>→</button>
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
    border-bottom: 1px solid #2a2a2a;
  }
  .title {
    font-weight: 600;
  }
  .status-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #666;
  }
  .status-dot.live {
    background: #3ecf6a;
  }
  .status-dot.reconnecting {
    background: #e0a72c;
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
    background: #14532d;
    color: #bbf7d0;
    padding: 0.2rem 0.5rem;
    border-radius: 4px;
  }
  .take-control {
    background: #2563eb;
    color: white;
    border: none;
    border-radius: 4px;
    padding: 0.3rem 0.6rem;
    cursor: pointer;
  }
  .notice {
    background: #3a2f0e;
    color: #ffd98a;
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
    background: #1e1e1e;
    border: 1px solid #333;
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
    border-top: 1px solid #2a2a2a;
  }
  .key-bar button {
    flex: 1;
    background: #1e1e1e;
    color: inherit;
    border: 1px solid #333;
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
