<script lang="ts">
  import { onDestroy, onMount, tick } from "svelte";
  import * as api from "./api";
  import { setControlling } from "./identity";
  import type { BrowseEntry, CreateSessionRequest, Preset, Session, SessionState } from "./types";

  let { onOpen }: { onOpen: (id: string) => void } = $props();

  let sessions: Session[] = $state([]);
  let presets: Preset[] = $state([]);
  let loading = $state(true);
  let loadError: string | null = $state(null);
  let searchQuery = $state("");

  // Status toggle alongside the text search below. "Active" (the default --
  // a finished session isn't what you're scanning for day to day) is
  // running|closing; "closed" is exited|lost, i.e. isDeletable's own split
  // further down. Not persisted, same as searchQuery -- reopening the page
  // is a fresh look at what's live now, not a resumed filter session.
  let statusFilter: "active" | "closed" = $state("active");

  function isActiveStatus(s: Session): boolean {
    return s.state === "running" || s.state === "closing";
  }

  let activeCount: number = $derived(sessions.filter(isActiveStatus).length);
  let closedCount: number = $derived(sessions.length - activeCount);

  // Client-side only -- the full list is already on hand from polling, and
  // a session count that ever justified a server-side search endpoint
  // instead would justify pagination first. Matches command or cwd
  // (against the real absolute path, not the "~/..." display string --
  // typing the username you already know shouldn't be punished for it).
  let filteredSessions: Session[] = $derived.by(() => {
    const byStatus = sessions.filter((s) => isActiveStatus(s) === (statusFilter === "active"));
    const q = searchQuery.trim().toLowerCase();
    if (!q) return byStatus;
    return byStatus.filter((s) => s.command.toLowerCase().includes(q) || s.cwd.toLowerCase().includes(q));
  });
  // Which machine this daemon is actually running on -- juggling more than
  // one teleportd (a dev box, a laptop, a work machine) otherwise looks
  // identical from this title alone; GET /api/v1/health already returns it
  // (daemon/src/device.rs: defaults to the hostname), this just displays
  // it. null while loading and left null on failure -- the plain "teleport"
  // title is a fine fallback, not worth a banner over.
  let deviceName: string | null = $state(null);
  // Same health() call as deviceName -- lets session-row cwds collapse back
  // to "~/..." instead of showing the full absolute path. null (no
  // collapsing, cwd shown in full) while loading, on failure, or if the
  // daemon couldn't resolve its own home directory.
  let homeDir: string | null = $state(null);

  let showLauncher = $state(false);
  let launching = $state(false);
  let launchError: string | null = $state(null);
  let selectedPreset = $state("");
  let customCommand = $state("/bin/sh");
  let cwd = $state("");
  // claude-preset-only, and deliberately not persisted/prefilled like cwd
  // is -- resuming is a one-off action on a specific launch, not a habit
  // worth remembering for the next one.
  let resumeSessionId = $state("");
  let firstFieldEl: HTMLSelectElement | undefined = $state();

  // Directory browser -- GET /api/v1/browse, an inline panel rather than a
  // separate route/modal component: it only ever matters while the
  // launcher itself is open, and closing the launcher already needs to
  // reset it (see closeLauncher below).
  let showBrowser = $state(false);
  let browsePath: string | null = $state(null);
  let browseParent: string | null = $state(null);
  let browseEntries: BrowseEntry[] = $state([]);
  let browseError: string | null = $state(null);
  let browseLoading = $state(false);

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

  // "/Users/aleh/projects/teleport" next to six other rows exactly like it
  // is mostly noise -- collapse it to "~/projects/teleport" the way a
  // shell prompt would, once we know the daemon's own home dir (homeDir is
  // null until loadHealthInfo() resolves, or forever if the daemon
  // couldn't determine one -- either way this is a no-op fallback, never
  // wrong, just less pretty). Matches only a real path-segment boundary
  // (homeDir itself, or homeDir + "/"), not an unrelated sibling directory
  // that merely starts with the same characters (e.g. "/Users/aleh-test").
  function displayCwd(cwd: string): string {
    if (!homeDir) return cwd;
    if (cwd === homeDir) return "~";
    if (cwd.startsWith(`${homeDir}/`)) return `~${cwd.slice(homeDir.length)}`;
    return cwd;
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
    await Promise.all([refresh(), loadPresets(), loadHealthInfo()]);
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
      // Claude Code is the common case -- default to it by id rather than
      // presets[0], so a reordered or hand-edited presets.toml (M8's
      // load_or_create writes the built-in default order, but nothing
      // pins it) can't silently change what a blank launcher submits to.
      const claude = presets.find((p) => p.id === "claude");
      if (claude) selectedPreset = claude.id;
      else if (presets.length > 0) selectedPreset = presets[0].id;
    } catch {
      // Presets are a convenience; the shell-command fallback still works.
    }
  }

  async function loadHealthInfo() {
    try {
      const res = await api.health();
      deviceName = res.device_name ?? null;
      homeDir = res.home_dir ?? null;
    } catch {
      // Same call the app already makes for other things; if it's failing
      // there's a bigger problem than the title, and that surfaces
      // elsewhere (loadError from refresh()). Not worth a second banner.
    }
  }

  async function openLauncher() {
    launchError = null;
    resumeSessionId = "";
    showLauncher = true;
    showBrowser = false;
    // Prefill with the last-used directory -- typing the same path every
    // launch is the friction this is meant to remove. Only when empty:
    // never clobber whatever the person is mid-typing across a reopen.
    if (!cwd && recentCwds.length > 0) cwd = recentCwds[0];
    await tick();
    firstFieldEl?.focus();
  }

  /** "Resume this" on a closed session with a known claude_resume_id -- opens the
      launcher already set up to continue that exact conversation instead of making
      the id be found, copied, and pasted in by hand. */
  async function openResumeLauncher(session: Session) {
    if (!session.claude_resume_id) return;
    launchError = null;
    showLauncher = true;
    // The launcher isn't a modal -- the list stays visible/tappable behind
    // it, so "Resume this" on a different row is reachable while an
    // earlier launcher session's browser panel is still open. Without
    // this it would linger, showing a stale directory listing for the cwd
    // this call is about to overwrite below.
    showBrowser = false;
    selectedPreset = "claude";
    resumeSessionId = session.claude_resume_id;
    cwd = session.cwd;
    await tick();
    firstFieldEl?.focus();
  }

  function closeLauncher() {
    showLauncher = false;
    showBrowser = false;
  }

  function onLauncherKeydown(e: KeyboardEvent) {
    if (e.key !== "Escape") return;
    if (showBrowser) closeBrowser();
    else closeLauncher();
  }

  async function loadBrowse(path?: string) {
    browseLoading = true;
    browseError = null;
    try {
      const res = await api.browse(path);
      browsePath = res.path;
      browseParent = res.parent;
      browseEntries = res.entries;
    } catch (e) {
      browseError = e instanceof Error ? e.message : String(e);
    } finally {
      browseLoading = false;
    }
  }

  function openBrowser() {
    showBrowser = true;
    // Start from whatever's already typed -- browsing is for refining a
    // starting point (recent cwd, hand-typed guess), not always starting
    // over from home. loadBrowse() itself falls back to the daemon's home
    // directory when given nothing.
    loadBrowse(cwd || undefined);
  }

  function closeBrowser() {
    showBrowser = false;
  }

  function useBrowsedFolder() {
    if (browsePath) cwd = browsePath;
    showBrowser = false;
  }

  function onLauncherSubmit(e: SubmitEvent) {
    e.preventDefault();
    launch();
  }

  async function launch() {
    launching = true;
    launchError = null;
    try {
      // Only claude actually understands `--resume`; the field itself is
      // hidden for any other preset, but the trim-and-check happens here
      // too so a stale value left over from switching presets mid-launcher
      // session can never leak into an unrelated command's argv.
      const resumeId = selectedPreset === "claude" ? resumeSessionId.trim() : "";
      const body: CreateSessionRequest = selectedPreset
        ? {
            kind: "agent",
            preset: selectedPreset,
            cwd: cwd || "/",
            cols: 120,
            rows: 36,
            ...(resumeId ? { args: ["--resume", resumeId] } : {}),
          }
        : { kind: "shell", command: customCommand, cwd: cwd || "/", cols: 120, rows: 36 };
      const created = await api.createSession(body);
      // The creator is the only client that could possibly be attached to a
      // session that didn't exist a moment ago -- the lease is unheld by
      // construction, so `mode=control` on the very next connect is granted
      // rather than falling back to observer (docs/04-api-protocol.md#control-lease:
      // "grants ... when the lease is free"). Without this, even the person
      // who just launched the session had to click "Take control" themselves,
      // and until they did, Terminal.svelte's observer path rendered the
      // fixed launch geometry letterboxed inside the window instead of
      // filling it.
      setControlling(created.id, true);
      showLauncher = false;
      showBrowser = false;
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

  // -- iOS-style swipe-to-reveal on a session row ----------------------
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
  // Only one row open at a time; REVEAL_PX must match .session-row__action's
  // width below (kept as plain numbers, not a shared CSS custom property --
  // it's one value, every use next to a comment pointing at the other).
  const REVEAL_PX = 72;
  const OPEN_THRESHOLD_PX = REVEAL_PX / 2;

  let openRowId: string | null = $state(null);
  let dragRowId: string | null = $state(null);
  let dragOffsetPx = $state(0);
  let touchStartX = 0;
  let touchStartY = 0;
  let touchDirection: "horizontal" | "vertical" | null = null;

  function closeSwipe() {
    openRowId = null;
  }

  /** The live transform for one row's front layer -- mid-drag, snapped open, or resting closed. */
  function rowOffset(sessionId: string): number {
    if (dragRowId === sessionId) return dragOffsetPx;
    return openRowId === sessionId ? -REVEAL_PX : 0;
  }

  function onRowTouchStart(e: TouchEvent, sessionId: string) {
    const touch = e.touches[0];
    if (!touch) return;
    touchStartX = touch.clientX;
    touchStartY = touch.clientY;
    touchDirection = null;
    dragRowId = sessionId;
    // Start from wherever this row already sits -- swiping an open row
    // shut feels continuous instead of jumping back to 0 first.
    dragOffsetPx = openRowId === sessionId ? -REVEAL_PX : 0;
  }

  function onRowTouchMove(e: TouchEvent, sessionId: string) {
    if (dragRowId !== sessionId) return;
    const touch = e.touches[0];
    if (!touch) return;
    const dx = touch.clientX - touchStartX;
    const dy = touch.clientY - touchStartY;
    if (touchDirection === null) {
      // A few px of wobble right at touchdown is normal on any gesture --
      // don't commit to horizontal (swipe) vs vertical (scroll) before the
      // direction is actually clear.
      if (Math.abs(dx) < 8 && Math.abs(dy) < 8) return;
      touchDirection = Math.abs(dx) > Math.abs(dy) ? "horizontal" : "vertical";
      if (touchDirection === "vertical") {
        // This is a page scroll, not a swipe -- let go and let the browser's
        // own native scrolling handle it from here (touch-action: pan-y
        // below keeps that native path unblocked while we're undecided).
        dragRowId = null;
        return;
      }
    }
    if (touchDirection !== "horizontal") return;
    e.preventDefault(); // committed to a horizontal swipe now -- stop the page scrolling along with it
    const base = openRowId === sessionId ? -REVEAL_PX : 0;
    dragOffsetPx = Math.min(0, Math.max(-REVEAL_PX, base + dx));
  }

  function onRowTouchEnd(sessionId: string) {
    if (dragRowId !== sessionId) return;
    dragRowId = null;
    openRowId = dragOffsetPx <= -OPEN_THRESHOLD_PX ? sessionId : null;
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
        {#if selectedPreset === "claude"}
          <label class="launcher__field">
            Resume session ID (optional)
            <!-- No format validation -- this is Claude Code's own opaque
                 conversation id, not something teleport has any business
                 parsing. A bad id surfaces as `claude --resume`'s own error,
                 same as any other agent CLI failure, right in the terminal. -->
            <input
              type="text"
              bind:value={resumeSessionId}
              placeholder="leave blank to start fresh"
              autocapitalize="none"
              autocorrect="off"
              spellcheck="false"
            />
          </label>
        {/if}
        <label class="launcher__field">
          Working directory
          <div class="launcher__cwd-row">
            <input
              type="text"
              bind:value={cwd}
              placeholder="/home/me/project"
              list="recent-cwds"
              autocapitalize="none"
              autocorrect="off"
              spellcheck="false"
            />
            <button type="button" class="btn launcher__browse-btn" onclick={openBrowser}>Browse…</button>
          </div>
          {#if recentCwds.length > 0}
            <datalist id="recent-cwds">
              {#each recentCwds as dir (dir)}
                <option value={dir}></option>
              {/each}
            </datalist>
          {/if}
        </label>
        {#if showBrowser}
          <!-- No keydown handler of its own -- it's inside the launcher <form>, so Escape
               already bubbles up to that form's own onLauncherKeydown, which checks
               showBrowser first. -->
          <div class="browser">
            <div class="browser__path">
              {#if browseLoading}
                Loading…
              {:else}
                {browsePath ?? "…"}
              {/if}
            </div>
            {#if browseError}
              <div class="banner banner--error" role="alert">{browseError}</div>
            {:else}
              <div class="browser__list">
                {#if browseParent}
                  <button type="button" class="browser__entry browser__entry--up" onclick={() => loadBrowse(browseParent ?? undefined)}>
                    .. (up)
                  </button>
                {/if}
                {#each browseEntries as entry (entry.path)}
                  <button type="button" class="browser__entry" onclick={() => loadBrowse(entry.path)}>
                    {entry.name}
                  </button>
                {/each}
                {#if !browseLoading && browseEntries.length === 0 && !browseParent}
                  <p class="browser__empty">No subdirectories here.</p>
                {/if}
              </div>
            {/if}
            <div class="launcher__actions">
              <button type="button" class="btn" onclick={closeBrowser}>Cancel</button>
              <button type="button" class="btn btn--primary" disabled={!browsePath} onclick={useBrowsedFolder}>
                Use this folder
              </button>
            </div>
          </div>
        {/if}
        {#if recentCwds.length > 0}
          <!-- datalist above covers typing; these are for tapping -- a
               datalist's dropdown affordance is inconsistent on mobile
               (docs/09-frontend.md#mobile), and re-typing a path you've
               already used is exactly the friction this removes. -->
          <div class="launcher__recent">
            {#each recentCwds as dir (dir)}
              <button type="button" class="cwd-chip" class:cwd-chip--active={dir === cwd} onclick={() => (cwd = dir)}>
                {dir}
              </button>
            {/each}
          </div>
        {/if}
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
      <ul class="session-list">
        {#each filteredSessions as session (session.id)}
          {@const isDeletable = session.state === "exited" || session.state === "lost"}
          <li class="session-row">
            <!-- svelte-ignore a11y_no_static_element_interactions -- a pure swipe-gesture
                 surface, not itself a control: the actual interactive elements are the <a>
                 link inside it and the .session-row__action button beside it, both fully
                 operable by keyboard/AT with no dependency on these touch handlers. -->
            <div
              class="session-row__front"
              style="transform: translateX({rowOffset(session.id)}px)"
              ontouchstart={(e) => onRowTouchStart(e, session.id)}
              ontouchmove={(e) => onRowTouchMove(e, session.id)}
              ontouchend={() => onRowTouchEnd(session.id)}
              ontouchcancel={() => onRowTouchEnd(session.id)}
            >
              <a
                class="session-row__link"
                href={`#/sessions/${session.id}`}
                onclick={(e) => {
                  if (openRowId === session.id) {
                    // Swiped open -- the tap dismisses the reveal instead of
                    // also navigating, same as tapping the content of an
                    // open iOS swipe action does.
                    e.preventDefault();
                    closeSwipe();
                    return;
                  }
                  onOpen(session.id);
                }}
              >
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
                {#if session.title}
                  <span class="session-row__title">({session.title})</span>
                {/if}
                <span class="session-row__cwd">{displayCwd(session.cwd)}</span>
                {#if session.controller}
                  <span class="session-row__controller">controlled by {session.controller}</span>
                {/if}
              </a>
              {#if isDeletable && session.claude_resume_id}
                <button
                  type="button"
                  class="session-row__resume"
                  onclick={() => openResumeLauncher(session)}
                >
                  ↻ Resume
                </button>
              {/if}
            </div>
            <button
              class="session-row__action"
              class:session-row__action--danger={isDeletable}
              aria-label={isDeletable ? "Delete session" : "Terminate session"}
              onclick={() => {
                closeSwipe();
                if (isDeletable) purge(session.id);
                else terminate(session.id);
              }}
            >
              <svg class="session-row__action-icon" viewBox="0 0 24 24" aria-hidden="true">
                <path d="M6 6l12 12M18 6L6 18" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" fill="none" />
              </svg>
            </button>
          </li>
        {/each}
      </ul>
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
  .launcher__recent {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin-top: -0.4rem;
  }
  .launcher__cwd-row {
    display: flex;
    gap: var(--space-2);
  }
  .launcher__cwd-row input {
    flex: 1;
    min-width: 0;
  }
  .launcher__browse-btn {
    flex-shrink: 0;
    font-size: 0.85rem;
  }

  /* Block: browser -- the inline GET /api/v1/browse directory picker,
     opened from .launcher__browse-btn. Sibling of launcher, not nested
     under it (it's a whole separate panel, not one of launcher's own
     fields), the same reasoning session-row is its own block rather than
     session-list's. */
  .browser {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    background: var(--surface-deep);
  }
  .browser__path {
    font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
    font-size: 0.85rem;
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .browser__list {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    max-height: 40vh;
    overflow-y: auto;
  }
  .browser__entry {
    text-align: left;
    background: none;
    border: none;
    border-radius: var(--radius-sm);
    padding: 0.45rem 0.6rem;
    font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
    font-size: 0.85rem;
    color: var(--fg);
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .browser__entry:hover {
    background: var(--surface-hover);
  }
  .browser__entry--up {
    color: var(--muted);
  }
  .browser__empty {
    margin: 0;
    padding: 0.45rem 0.6rem;
    color: var(--muted);
    font-size: 0.85rem;
  }

  /* Block: cwd-chip -- a tap-to-fill recent working directory (sibling of
     launcher, not launcher__recent__chip: BEM elements don't nest). */
  .cwd-chip {
    background: var(--surface);
    color: var(--muted);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    padding: 0.3rem 0.55rem;
    font-size: 0.78rem;
    font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    cursor: pointer;
    transition:
      background-color var(--transition-fast),
      border-color var(--transition-fast),
      color var(--transition-fast);
  }
  .cwd-chip:hover {
    border-color: var(--muted);
    color: var(--fg);
  }
  .cwd-chip--active {
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
</style>
