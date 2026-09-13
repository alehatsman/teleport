<script lang="ts">
  import { onDestroy, onMount } from "svelte"
  import {
    createSession,
    deleteSession,
    describeError,
    health,
    listPresets,
    listSessions,
  } from "@/api/api"
  import { setControlling } from "@/api/identity"
  import type { CreateSessionRequest, Preset, Session } from "@/api/types"
  import SessionLauncher from "@/SessionLauncher.svelte"
  import SessionList from "@/SessionList.svelte"

  let { onOpen }: { onOpen: (id: string) => void } = $props()

  let sessions: Session[] = $state([])
  // Refreshed on every poll so the per-row age ticks over without each
  // row re-reading Date.now() (a $derived over a non-reactive clock never
  // re-runs). Coarse on purpose -- ages are shown at minute granularity.
  let now = $state(Date.now())
  let presets: Preset[] = $state([])
  let loading = $state(true)
  let loadError: string | null = $state(null)
  // Has any session-list fetch ever succeeded? Gates the "No sessions yet"
  // placeholder: until it has, an empty `sessions` means "unknown", not
  // "none" -- rendering the empty state under an error banner (bad token,
  // daemon down) told the reader there was nothing running when we simply
  // couldn't ask.
  let loadedOnce = $state(false)
  let searchQuery = $state("")

  // Status toggle alongside the text search below. "Active" (the default --
  // a finished session isn't what you're scanning for day to day) is
  // running|closing; "closed" is exited|lost. Not persisted, same as
  // searchQuery -- reopening the page is a fresh look at what's live now,
  // not a resumed filter session.
  let statusFilter: "active" | "closed" = $state("active")

  function isActiveStatus(s: Session): boolean {
    return s.state === "running" || s.state === "closing"
  }

  let activeCount: number = $derived(sessions.filter(isActiveStatus).length)
  let closedCount: number = $derived(sessions.length - activeCount)

  // Client-side only -- the full list is already on hand from polling, and
  // a session count that ever justified a server-side search endpoint
  // instead would justify pagination first. Matches command or cwd
  // (against the real absolute path, not the "~/..." display string --
  // typing the username you already know shouldn't be punished for it).
  let filteredSessions: Session[] = $derived.by(() => {
    const byStatus = sessions.filter((s) => isActiveStatus(s) === (statusFilter === "active"))
    const q = searchQuery.trim().toLowerCase()
    if (!q) return byStatus
    return byStatus.filter(
      (s) => s.command.toLowerCase().includes(q) || s.cwd.toLowerCase().includes(q)
    )
  })
  // Which machine this daemon is actually running on -- juggling more than
  // one teleportd (a dev box, a laptop, a work machine) otherwise looks
  // identical from this title alone; GET /api/v1/health already returns it
  // (daemon/src/device.rs: defaults to the hostname), this just displays
  // it. null while loading and left null on failure -- the plain "teleport"
  // title is a fine fallback, not worth a banner over.
  let deviceName: string | null = $state(null)
  // Same health() call as deviceName -- lets SessionRow's cwds collapse
  // back to "~/..." instead of showing the full absolute path. null (no
  // collapsing, cwd shown in full) while loading, on failure, or if the
  // daemon couldn't resolve its own home directory.
  let homeDir: string | null = $state(null)
  // Set once health() has answered. Until then every poll tick retries it:
  // a single failed fetch at mount otherwise left the host name blank and
  // every cwd un-collapsed until a full reload.
  let healthLoaded = $state(false)

  let showLauncher = $state(false)
  // Owned by Sessions.svelte, not SessionLauncher, and passed down bindable --
  // these three outlive the launcher panel's own mount/unmount cycle
  // (SessionLauncher only exists in the DOM while showLauncher is true).
  // cwd in particular must never reset itself out from under someone
  // mid-typing across a close+reopen; selectedPreset/customCommand follow
  // the same rule for consistency.
  let selectedPreset = $state("")
  let customCommand = $state("/bin/sh")
  let cwd = $state("")
  // Set right before showLauncher flips true by openResumeLauncher below;
  // null for a plain "New session" open. SessionLauncher reads it once, at
  // its own creation, to prefill preset/resume-id/cwd for that one open.
  let resumeSessionForLauncher: Session | null = $state(null)

  // M8 (docs/11-mvp-plan.md#m8--agent-presets): recent working directories,
  // derived from the session list already on hand -- no new storage/endpoint.
  // Most-recent-use-first, deduped, capped so the launcher's dropdown stays
  // scannable.
  let recentCwds: string[] = $derived.by(() => {
    const latest = new Map<string, number>()
    for (const s of sessions) {
      if (!s.cwd) continue
      const prev = latest.get(s.cwd)
      if (prev === undefined || s.created_at_ms > prev) latest.set(s.cwd, s.created_at_ms)
    }
    return [...latest.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 8)
      .map(([dir]) => dir)
  })

  let pollTimer: ReturnType<typeof setInterval> | null = null

  onMount(async () => {
    await Promise.all([refresh(), loadPresets(), loadHealthInfo()])
    loading = false
    // D2 (docs/15-open-questions.md#d2--session-list-freshness) is still an
    // open decision -- polling is the pragmatic interim answer for M5, not
    // a considered final one. Flagged, not silently closed.
    pollTimer = setInterval(refresh, 3000)
  })

  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer)
  })

  async function refresh() {
    try {
      const res = await listSessions()
      sessions = res.sessions
      now = Date.now()
      loadError = null
      loadedOnce = true
      if (!healthLoaded) void loadHealthInfo()
    } catch (e) {
      loadError = describeError(e)
    }
  }

  async function loadPresets() {
    try {
      const res = await listPresets()
      presets = res.presets
      // Claude Code is the common case -- default to it by id rather than
      // presets[0], so a reordered or hand-edited presets.toml (M8's
      // load_or_create writes the built-in default order, but nothing
      // pins it) can't silently change what a blank launcher submits to.
      const claude = presets.find((p) => p.id === "claude")
      const first = presets[0]
      if (claude) selectedPreset = claude.id
      else if (first !== undefined) selectedPreset = first.id
    } catch {
      // Presets are a convenience; the shell-command fallback still works.
    }
  }

  async function loadHealthInfo() {
    try {
      const res = await health()
      deviceName = res.device_name ?? null
      homeDir = res.home_dir ?? null
      healthLoaded = true
    } catch {
      // Same call the app already makes for other things; if it's failing
      // there's a bigger problem than the title, and that surfaces
      // elsewhere (loadError from refresh()). Not worth a second banner --
      // the next successful poll retries this instead.
    }
  }

  function openLauncher() {
    resumeSessionForLauncher = null
    showLauncher = true
  }

  /** "Resume this" on a closed session with a known claude_resume_id -- opens the
      launcher already set up to continue that exact conversation instead of making
      the id be found, copied, and pasted in by hand. */
  function openResumeLauncher(session: Session) {
    if (!session.claude_resume_id) return
    resumeSessionForLauncher = session
    showLauncher = true
  }

  function closeLauncher() {
    showLauncher = false
  }

  async function handleLaunch(body: CreateSessionRequest) {
    const created = await createSession(body)
    // The creator is the only client that could possibly be attached to a
    // session that didn't exist a moment ago -- the lease is unheld by
    // construction, so `mode=control` on the very next connect is granted
    // rather than falling back to observer (docs/04-api-protocol.md#control-lease:
    // "grants ... when the lease is free"). Without this, even the person
    // who just launched the session had to click "Take control" themselves,
    // and until they did, Terminal.svelte's observer path rendered the
    // fixed launch geometry letterboxed inside the window instead of
    // filling it.
    setControlling(created.id, true)
    showLauncher = false
    onOpen(created.id)
  }

  async function terminate(id: string) {
    // Kills a running process -- an agent mid-task, a shell with state. Not
    // irreversible the way purge is (the log survives), but the X sits in
    // the same slot as delete and one mis-click ends real work. Same plain
    // confirm() as purge, for the same reason.
    if (!confirm("Terminate this session? The running process will be killed.")) return
    try {
      await deleteSession(id)
      await refresh()
    } catch (e) {
      loadError = describeError(e)
    }
  }

  async function purge(id: string) {
    // Purge also deletes the on-disk log (api/api.ts) -- the one irreversible
    // action in this app. One confirm, not a custom modal: boring and it
    // still stops a mis-tap.
    if (!confirm("Delete this session and its log? This can't be undone.")) return
    try {
      await deleteSession(id, true)
      await refresh()
    } catch (e) {
      loadError = describeError(e)
    }
  }
</script>

<div class="sessions">
  <header class="sessions__header">
    <h1 class="sessions__title">
      <span class="sessions__prompt" aria-hidden="true">&rsaquo;</span>teleport{#if deviceName}<span
          class="sessions__host">&nbsp;(host: {deviceName})</span>{/if}
    </h1>
    <button
      class="btn btn--primary sessions__new-btn"
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
      <SessionLauncher
        bind:cwd
        bind:selectedPreset
        bind:customCommand
        {presets}
        {recentCwds}
        {homeDir}
        resumeSession={resumeSessionForLauncher}
        onLaunch={handleLaunch}
        onClose={closeLauncher}
      />
    {/if}

    {#if loading}
      <p class="sessions__loading">Loading…</p>
    {:else if loadedOnce && sessions.length === 0}
      <div class="empty">
        <p class="empty__text">No sessions yet.</p>
        <button class="btn btn--primary" onclick={openLauncher}>New session</button>
      </div>
    {:else}
      <div class="status-toggle" role="tablist" aria-label="Filter by status">
        <button
          type="button"
          role="tab"
          aria-selected={statusFilter === "active"}
          class="status-toggle__option"
          class:status-toggle__option--active={statusFilter === "active"}
          onclick={() => (statusFilter = "active")}
        >
          Active ({activeCount})
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={statusFilter === "closed"}
          class="status-toggle__option"
          class:status-toggle__option--active={statusFilter === "closed"}
          onclick={() => (statusFilter = "closed")}
        >
          Closed ({closedCount})
        </button>
      </div>
      <input
        type="search"
        class="sessions__search"
        placeholder="Search sessions…"
        aria-label="Search sessions"
        bind:value={searchQuery}
        autocapitalize="none"
        autocorrect="off"
        spellcheck="false"
      />
      {#if filteredSessions.length === 0}
        <p class="sessions__loading">
          {#if searchQuery.trim()}
            No {statusFilter} sessions match "{searchQuery.trim()}".
          {:else}
            No {statusFilter} sessions.
          {/if}
        </p>
      {/if}
      <SessionList
        sessions={filteredSessions}
        {now}
        {homeDir}
        {onOpen}
        onResume={openResumeLauncher}
        onTerminate={terminate}
        onPurge={purge}
      />
    {/if}
  </main>

  {#if !showLauncher}
    <!-- Fixed, thumb-reachable twin of .sessions__new-btn -- same openLauncher(),
         just easier to hit one-handed on a phone than the header. Hidden while
         the launcher panel is open: no point stacking two "add" affordances,
         and it would otherwise sit on top of the panel's own buttons on a
         short mobile viewport. -->
    <button
      class="fab"
      onclick={openLauncher}
      aria-expanded={showLauncher}
      aria-controls="launcher-panel"
      aria-label="New session"
    >
      <svg class="fab__icon" viewBox="0 0 24 24" aria-hidden="true">
        <path d="M12 5v14M5 12h14" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" fill="none" />
      </svg>
    </button>
  {/if}
</div>

<style>
  /* Block: sessions -- the session-list view (root). */
  .sessions {
    padding: var(--space-4);
    /* Room for .fab (56px + its own bottom offset) so it never sits on top
       of the last session row. */
    padding-bottom: calc(56px + var(--space-4) * 2);
    max-width: 720px;
    margin: 0 auto;
  }
  .sessions__header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 1.75rem;
  }
  .sessions__title {
    font-size: 1.2rem;
    font-weight: 700;
    letter-spacing: 0.01em;
    margin: 0;
    font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
    /* The host suffix is a real hostname -- unbounded length -- unlike the
       literal "teleport" this used to be alone. overflow:hidden gives this
       flex item an automatic min-width of 0 (Session.svelte's control-btn
       fix hit the same flexbox rule), so a long one ellipsizes instead of
       pushing "New session" off the header or wrapping to a second line. */
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sessions__prompt {
    color: var(--accent);
    margin-right: 0.3rem;
  }
  .sessions__host {
    font-weight: 400;
    opacity: 0.6;
  }
  .sessions__new-btn {
    /* Fixed, short label -- let the title (unbounded host name) be the one
       that shrinks; this never should. */
    flex-shrink: 0;
  }
  .sessions__loading {
    opacity: 0.6;
  }
  .sessions__search {
    display: block;
    width: 100%;
    margin-bottom: var(--space-3);
    font-size: 0.9rem;
  }
  .status-toggle {
    display: flex;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }
  .status-toggle__option {
    background: var(--surface);
    color: var(--muted);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    padding: 0.3rem 0.65rem;
    font-size: 0.8rem;
    cursor: pointer;
  }
  .status-toggle__option:hover {
    border-color: var(--muted);
    color: var(--fg);
  }
  .status-toggle__option--active {
    background: var(--surface-hover);
    border-color: var(--accent);
    color: var(--fg);
  }

  /* Block: empty -- the no-sessions-yet placeholder. */
  .empty {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-3);
    padding: 2.5rem var(--space-3);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
  }
  .empty__text {
    margin: 0;
    opacity: 0.6;
  }

  @media (max-width: 600px) {
    .sessions {
      padding: 0.5rem;
      padding-bottom: calc(56px + var(--space-4) * 2);
    }
  }

  /* Block: fab -- fixed, always-reachable "new session" button; a thumb-zone
     twin of .sessions__new-btn, not a replacement (mouse users keep the
     header button; this is for one-handed phone use). Round, gradient +
     shadow borrowed straight from .btn--primary / --shadow-panel rather than
     inventing a second visual language for "primary action". */
  .fab {
    position: fixed;
    right: var(--space-4);
    /* env() falls back to 0 with no viewport-fit=cover meta, same as not
       being there at all -- safe to always include. */
    bottom: calc(var(--space-4) + env(safe-area-inset-bottom, 0px));
    width: 56px;
    height: 56px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    border: none;
    cursor: pointer;
    background: linear-gradient(180deg, var(--accent-hover), var(--accent));
    color: var(--accent-fg);
    box-shadow: var(--shadow-panel);
    z-index: 5; /* above list content, below .toast's 10 */
    transition:
      transform var(--transition-fast),
      box-shadow var(--transition-fast);
  }
  .fab:hover {
    box-shadow: var(--shadow-glow);
    transform: translateY(-2px);
  }
  .fab:active {
    transform: translateY(0) scale(0.94);
  }
  .fab__icon {
    width: 26px;
    height: 26px;
  }
  /* A thumb affordance. With a mouse the header button is one short move
     away and the FAB was a third "New session" on an empty screen -- same
     input-type split SessionRow's swipe action uses. */
  @media (hover: hover) and (pointer: fine) {
    .fab {
      display: none;
    }
  }
</style>
