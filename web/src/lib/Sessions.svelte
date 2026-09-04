<script lang="ts">
  import { onDestroy, onMount, tick } from "svelte";
  import * as api from "./api";
  import type { CreateSessionRequest, Preset, Session, SessionState } from "./types";

  let { onOpen }: { onOpen: (id: string) => void } = $props();

  let sessions: Session[] = $state([]);
  let presets: Preset[] = $state([]);
  let loading = $state(true);
  let loadError: string | null = $state(null);

  let showLauncher = $state(false);
  let launching = $state(false);
  let launchError: string | null = $state(null);
  let selectedPreset = $state("");
  let customCommand = $state("/bin/sh");
  let cwd = $state("");
  let firstFieldEl: HTMLSelectElement | undefined = $state();

  let pollTimer: ReturnType<typeof setInterval> | null = null;

  const STATE_LABELS: Record<SessionState, string> = {
    running: "Running",
    closing: "Closing",
    exited: "Exited",
    lost: "Lost",
  };

  // D3 (docs/04-api-protocol.md#get-apiv1sessions):
  // idle_since_ms is already a live signal (the daemon clears it the moment
  // output resumes), but last_bell_ms never clears server-side -- one bell
  // three hours ago shouldn't glow forever. Bound it to a recency window
  // here instead of teaching the daemon an "acknowledged" concept for M8.
  const BELL_RECENCY_MS = 2 * 60 * 1000;

  function needsAttention(s: Session): boolean {
    if (s.state !== "running") return false;
    if (s.idle_since_ms !== null) return true;
    return s.last_bell_ms !== null && Date.now() - s.last_bell_ms < BELL_RECENCY_MS;
  }

  // M8 (docs/11-mvp-plan.md#m8--agent-presets): recent working directories,
  // derived from the session list already on hand -- no new storage/endpoint.
  // Most-recent-use-first, deduped, capped so the dropdown stays scannable.
  let recentCwds: string[] = $derived.by(() => {
    const latest = new Map<string, number>();
    for (const s of sessions) {
      if (!s.cwd) continue;
      const prev = latest.get(s.cwd);
      if (prev === undefined || s.created_at_ms > prev) latest.set(s.cwd, s.created_at_ms);
    }
    return [...latest.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 8)
      .map(([dir]) => dir);
  });

  onMount(async () => {
    await Promise.all([refresh(), loadPresets()]);
    loading = false;
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

  async function openLauncher() {
    launchError = null;
    showLauncher = true;
    await tick();
    firstFieldEl?.focus();
  }

  function closeLauncher() {
    showLauncher = false;
  }

  function onLauncherKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") closeLauncher();
  }

  function onLauncherSubmit(e: SubmitEvent) {
    e.preventDefault();
    launch();
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
    // Purge also deletes the on-disk log (api.ts) -- the one irreversible
    // action in this app. One confirm, not a custom modal: boring and it
    // still stops a mis-tap.
    if (!confirm("Delete this session and its log? This can't be undone.")) return;
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
    <button class="primary" onclick={openLauncher} aria-expanded={showLauncher} aria-controls="launcher-panel">
      New session
    </button>
  </header>

  <main>
    {#if loadError}
      <div class="banner error" role="alert">{loadError}</div>
    {/if}

    {#if showLauncher}
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -- Escape-to-close on the
           container, standard for a form acting as a dismissable panel; the actual controls
           inside remain focusable, interactive elements. -->
      <form id="launcher-panel" class="launcher" onsubmit={onLauncherSubmit} onkeydown={onLauncherKeydown}>
        <label>
          Preset
          <select bind:value={selectedPreset} bind:this={firstFieldEl}>
            <option value="">Custom command</option>
            {#each presets as preset (preset.id)}
              <option value={preset.id}>{preset.label}</option>
            {/each}
          </select>
        </label>
        {#if !selectedPreset}
          <label>
            Command
            <input type="text" bind:value={customCommand} autocapitalize="none" autocorrect="off" spellcheck="false" />
          </label>
        {/if}
        <label>
          Working directory
          <input
            type="text"
            bind:value={cwd}
            placeholder="/home/me/project"
            list="recent-cwds"
            autocapitalize="none"
            autocorrect="off"
            spellcheck="false"
          />
          {#if recentCwds.length > 0}
            <datalist id="recent-cwds">
              {#each recentCwds as dir (dir)}
                <option value={dir}></option>
              {/each}
            </datalist>
          {/if}
        </label>
        {#if launchError}
          <div class="banner error" role="alert">{launchError}</div>
        {/if}
        <div class="actions">
          <button type="button" onclick={closeLauncher} disabled={launching}>Cancel</button>
          <button type="submit" class="primary" disabled={launching}>{launching ? "Launching…" : "Launch"}</button>
        </div>
      </form>
    {/if}

    {#if loading}
      <p class="empty">Loading…</p>
    {:else if sessions.length === 0}
      <p class="empty">No sessions yet.</p>
    {:else}
      <ul class="session-list">
        {#each sessions as session (session.id)}
          <li class="session-row">
            <a class="open" href={`#/sessions/${session.id}`} onclick={() => onOpen(session.id)}>
              <span
                class="state"
                aria-hidden="true"
                class:running={session.state === "running"}
                class:exited={session.state === "exited"}
                class:lost={session.state === "lost"}
              ></span>
              <span class="sr-only">{STATE_LABELS[session.state]}.</span>
              {#if needsAttention(session)}
                <span class="attention" aria-hidden="true">●</span>
                <span class="sr-only">Needs attention.</span>
              {/if}
              <span class="command">{session.command}</span>
              <span class="cwd">{session.cwd}</span>
              {#if session.controller}
                <span class="controller">controlled by {session.controller}</span>
              {/if}
            </a>
            {#if session.state === "exited" || session.state === "lost"}
              <button class="danger" onclick={() => purge(session.id)}>Delete</button>
            {:else}
              <button onclick={() => terminate(session.id)}>Terminate</button>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </main>
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
    background: var(--error-bg);
    color: var(--error-fg);
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
    margin-bottom: 0.75rem;
  }
  .launcher {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    border: 1px solid var(--border-strong);
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
    border: 1px solid var(--border);
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
    text-decoration: none;
    color: inherit;
    cursor: pointer;
    padding: 0;
    overflow: hidden;
  }
  .state {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--muted);
    flex-shrink: 0;
  }
  .state.running {
    background: var(--success);
  }
  .state.exited {
    background: var(--muted);
  }
  .state.lost {
    background: var(--warning);
  }
  .attention {
    color: var(--attention);
    font-size: 0.7rem;
    flex-shrink: 0;
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
    background: var(--accent);
    color: var(--accent-fg);
    border: none;
  }
  button.danger {
    background: var(--danger-bg);
    color: var(--danger-fg);
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
