<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import * as api from "./api";
  import type { CreateSessionRequest, Preset, Session } from "./types";

  let { onOpen }: { onOpen: (id: string) => void } = $props();

  let sessions: Session[] = $state([]);
  let presets: Preset[] = $state([]);
  let loadError: string | null = $state(null);

  let showLauncher = $state(false);
  let launching = $state(false);
  let launchError: string | null = $state(null);
  let selectedPreset = $state("");
  let customCommand = $state("/bin/sh");
  let cwd = $state("");

  let pollTimer: ReturnType<typeof setInterval> | null = null;

  onMount(async () => {
    await Promise.all([refresh(), loadPresets()]);
    // D2 (docs/15-open-questions.md#d2--session-list-freshness) is still an
    // open decision -- polling is the pragmatic interim answer for M5, not
    // a considered final one. Flagged, not silently closed.
    pollTimer = setInterval(refresh, 3000);
  });

  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer);
  });

  async function refresh() {
    try {
      const res = await api.listSessions();
      sessions = res.sessions;
      loadError = null;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  }

  async function loadPresets() {
    try {
      const res = await api.listPresets();
      presets = res.presets;
      if (presets.length > 0) selectedPreset = presets[0].id;
    } catch {
      // Presets are a convenience; the shell-command fallback still works.
    }
  }

  function openLauncher() {
    launchError = null;
    showLauncher = true;
  }

  async function launch() {
    launching = true;
    launchError = null;
    try {
      const body: CreateSessionRequest = selectedPreset
        ? { kind: "agent", preset: selectedPreset, cwd: cwd || "/", cols: 120, rows: 36 }
        : { kind: "shell", command: customCommand, cwd: cwd || "/", cols: 120, rows: 36 };
      const created = await api.createSession(body);
      showLauncher = false;
      onOpen(created.id);
    } catch (e) {
      launchError = e instanceof Error ? e.message : String(e);
    } finally {
      launching = false;
    }
  }

  async function terminate(id: string) {
    try {
      await api.deleteSession(id);
      await refresh();
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  }

  async function purge(id: string) {
    try {
      await api.deleteSession(id, true);
      await refresh();
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div class="sessions-view">
  <header>
    <h1>teleport</h1>
    <button class="primary" onclick={openLauncher}>New session</button>
  </header>

  {#if loadError}
    <div class="banner error">{loadError}</div>
  {/if}

  {#if showLauncher}
    <div class="launcher">
      <label>
        Preset
        <select bind:value={selectedPreset}>
          <option value="">Custom command</option>
          {#each presets as preset (preset.id)}
            <option value={preset.id}>{preset.label}</option>
          {/each}
        </select>
      </label>
      {#if !selectedPreset}
        <label>
          Command
          <input type="text" bind:value={customCommand} />
        </label>
      {/if}
      <label>
        Working directory
        <input type="text" bind:value={cwd} placeholder="/home/me/project" />
      </label>
      {#if launchError}
        <div class="banner error">{launchError}</div>
      {/if}
      <div class="actions">
        <button onclick={() => (showLauncher = false)} disabled={launching}>Cancel</button>
        <button class="primary" onclick={launch} disabled={launching}>{launching ? "Launching…" : "Launch"}</button>
      </div>
    </div>
  {/if}

  {#if sessions.length === 0}
    <p class="empty">No sessions yet.</p>
  {:else}
    <ul class="session-list">
      {#each sessions as session (session.id)}
        <li class="session-row">
          <button class="open" onclick={() => onOpen(session.id)}>
            <span
              class="state"
              class:running={session.state === "running"}
              class:exited={session.state === "exited"}
              class:lost={session.state === "lost"}
            ></span>
            <span class="command">{session.command}</span>
            <span class="cwd">{session.cwd}</span>
            {#if session.controller}
              <span class="controller">controlled by {session.controller}</span>
            {/if}
          </button>
          {#if session.state === "exited" || session.state === "lost"}
            <button class="danger" onclick={() => purge(session.id)}>Delete</button>
          {:else}
            <button onclick={() => terminate(session.id)}>Terminate</button>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</div>

<style>
  .sessions-view {
    padding: 1rem;
    max-width: 720px;
    margin: 0 auto;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 1rem;
  }
  h1 {
    font-size: 1.1rem;
    margin: 0;
  }
  .banner.error {
    background: #4a1414;
    color: #ffb4b4;
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
    margin-bottom: 0.75rem;
  }
  .launcher {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    border: 1px solid #333;
    border-radius: 6px;
    padding: 0.75rem;
    margin-bottom: 1rem;
  }
  .launcher label {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: 0.85rem;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
  }
  .empty {
    opacity: 0.6;
  }
  .session-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .session-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    border: 1px solid #2a2a2a;
    border-radius: 6px;
    padding: 0.5rem 0.75rem;
  }
  .open {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    background: none;
    border: none;
    text-align: left;
    color: inherit;
    cursor: pointer;
    padding: 0;
    overflow: hidden;
  }
  .state {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #666;
    flex-shrink: 0;
  }
  .state.running {
    background: #3ecf6a;
  }
  .state.exited {
    background: #666;
  }
  .state.lost {
    background: #c9a227;
  }
  .command {
    font-weight: 600;
  }
  .cwd {
    opacity: 0.6;
    font-size: 0.85rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .controller {
    margin-left: auto;
    font-size: 0.75rem;
    opacity: 0.7;
  }
  button.primary {
    background: #2563eb;
    color: white;
    border: none;
  }
  button.danger {
    background: #7f1d1d;
    color: #ffdada;
    border: none;
  }
  button {
    border-radius: 4px;
    padding: 0.4rem 0.7rem;
    cursor: pointer;
  }

  @media (max-width: 600px) {
    .sessions-view {
      padding: 0.5rem;
    }
  }
</style>
