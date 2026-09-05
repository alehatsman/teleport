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

<div class="sessions">
  <header class="sessions__header">
    <h1 class="sessions__title">teleport</h1>
    <button
      class="btn btn--primary"
      onclick={openLauncher}
      aria-expanded={showLauncher}
      aria-controls="launcher-panel"
    >
      New session
    </button>
  </header>

  <main>
    {#if loadError}
      <div class="banner banner--error" role="alert">{loadError}</div>
    {/if}

    {#if showLauncher}
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -- Escape-to-close on the
           container, standard for a form acting as a dismissable panel; the actual controls
           inside remain focusable, interactive elements. -->
      <form id="launcher-panel" class="launcher" onsubmit={onLauncherSubmit} onkeydown={onLauncherKeydown}>
        <label class="launcher__field">
          Preset
          <select bind:value={selectedPreset} bind:this={firstFieldEl}>
            <option value="">Custom command</option>
            {#each presets as preset (preset.id)}
              <option value={preset.id}>{preset.label}</option>
            {/each}
          </select>
        </label>
        {#if !selectedPreset}
          <label class="launcher__field">
            Command
            <input type="text" bind:value={customCommand} autocapitalize="none" autocorrect="off" spellcheck="false" />
          </label>
        {/if}
        <label class="launcher__field">
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
          <div class="banner banner--error" role="alert">{launchError}</div>
        {/if}
        <div class="launcher__actions">
          <button type="button" class="btn" onclick={closeLauncher} disabled={launching}>Cancel</button>
          <button type="submit" class="btn btn--primary" disabled={launching}>
            {launching ? "Launching…" : "Launch"}
          </button>
        </div>
      </form>
    {/if}

    {#if loading}
      <p class="sessions__loading">Loading…</p>
    {:else if sessions.length === 0}
      <div class="empty">
        <p class="empty__text">No sessions yet.</p>
        <button class="btn btn--primary" onclick={openLauncher}>New session</button>
      </div>
    {:else}
      <ul class="session-list">
        {#each sessions as session (session.id)}
          <li class="session-row">
            <a class="session-row__link" href={`#/sessions/${session.id}`} onclick={() => onOpen(session.id)}>
              <span
                class="dot"
                aria-hidden="true"
                class:dot--success={session.state === "running"}
                class:dot--warning={session.state === "lost"}
              ></span>
              <span class="sr-only">{STATE_LABELS[session.state]}.</span>
              {#if needsAttention(session)}
                <span class="session-row__attention" aria-hidden="true">●</span>
                <span class="sr-only">Needs attention.</span>
              {/if}
              <span class="session-row__command">{session.command}</span>
              <span class="session-row__cwd">{session.cwd}</span>
              {#if session.controller}
                <span class="session-row__controller">controlled by {session.controller}</span>
              {/if}
            </a>
            {#if session.state === "exited" || session.state === "lost"}
              <button class="btn btn--danger" onclick={() => purge(session.id)}>Delete</button>
            {:else}
              <button class="btn" onclick={() => terminate(session.id)}>Terminate</button>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </main>
</div>

<style>
  /* Block: sessions -- the session-list view (root). */
  .sessions {
    padding: var(--space-4);
    max-width: 720px;
    margin: 0 auto;
  }
  .sessions__header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 1.25rem;
    padding-bottom: var(--space-3);
    border-bottom: 1px solid var(--border);
  }
  .sessions__title {
    font-size: 1.15rem;
    font-weight: 700;
    letter-spacing: 0.02em;
    margin: 0;
  }
  .sessions__loading {
    opacity: 0.6;
  }

  /* Block: launcher -- the new-session form panel. */
  .launcher {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    background: var(--surface-raised);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-lg);
    padding: var(--space-4);
    margin-bottom: var(--space-4);
    box-shadow: var(--shadow-panel);
  }
  .launcher__field {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    font-size: 0.85rem;
    color: var(--muted);
  }
  .launcher__field input,
  .launcher__field select {
    color: var(--fg);
  }
  .launcher__actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }

  /* Block: empty -- the no-sessions-yet placeholder. */
  .empty {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-3);
    padding: 2.5rem var(--space-3);
    border: 1px dashed var(--border-strong);
    border-radius: var(--radius-lg);
  }
  .empty__text {
    margin: 0;
    opacity: 0.6;
  }

  /* Block: session-list -- just the list container; each item is its own
     block (session-row) since it has too many parts to stay one element
     deep. */
  .session-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  /* Block: session-row -- one row in the session list. */
  .session-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 0.6rem var(--space-3);
    transition:
      border-color var(--transition-fast),
      background-color var(--transition-fast);
  }
  .session-row:hover {
    border-color: var(--border-strong);
    background: var(--surface-hover);
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
  }
  .session-row__attention {
    color: var(--attention);
    font-size: 0.7rem;
    flex-shrink: 0;
  }
  .session-row__command {
    font-weight: 600;
    font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
    font-size: 0.9rem;
  }
  .session-row__cwd {
    opacity: 0.55;
    font-size: 0.8rem;
    font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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

  @media (max-width: 600px) {
    .sessions {
      padding: 0.5rem;
    }
  }
</style>
