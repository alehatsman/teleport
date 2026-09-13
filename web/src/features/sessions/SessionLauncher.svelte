<script lang="ts">
  import { onMount, tick } from "svelte"
  import { describeError } from "@/api/api"
  import type { CreateSessionRequest, Preset, Session } from "@/api/types"
  import DirectoryBrowser from "@/features/sessions/DirectoryBrowser.svelte"

  // The new-session form panel. `cwd`/`selectedPreset`/`customCommand` are
  // owned by Sessions.svelte and bound, not local state here -- they must
  // outlive this component's own mount/unmount cycle (it only exists in the
  // DOM while the panel is open) so a value the user already typed or chose
  // is never lost on a close+reopen. Everything else here resets on every
  // open, matching a fresh mount.
  let {
    cwd = $bindable(),
    selectedPreset = $bindable(),
    customCommand = $bindable(),
    presets,
    recentCwds,
    homeDir,
    resumeSession,
    onLaunch,
    onClose,
  }: {
    cwd: string
    selectedPreset: string
    customCommand: string
    presets: Preset[]
    recentCwds: string[]
    homeDir: string | null
    resumeSession: Session | null
    onLaunch: (req: CreateSessionRequest) => Promise<void>
    onClose: () => void
  } = $props()

  let launching = $state(false)
  let launchError: string | null = $state(null)
  // claude-preset-only, and deliberately not persisted/prefilled like cwd
  // is -- resuming is a one-off action on a specific launch, not a habit
  // worth remembering for the next one. Set in onMount below, not from
  // `resumeSession` directly here, so it's a plain one-time read rather
  // than a reactive dependency on a prop this component never revisits.
  let resumeSessionId = $state("")
  let firstFieldEl: HTMLSelectElement | undefined = $state()
  let showBrowser = $state(false)

  onMount(async () => {
    if (resumeSession) {
      // "Resume this" on a closed session -- set up to continue that exact
      // conversation instead of making the id be found, copied, and pasted
      // in by hand.
      selectedPreset = "claude"
      resumeSessionId = resumeSession.claude_resume_id ?? ""
      cwd = resumeSession.cwd
    } else if (!cwd && recentCwds[0] !== undefined) {
      // Prefill with the last-used directory -- typing the same path every
      // launch is the friction this is meant to remove. Only when empty:
      // never clobber whatever was carried over from an earlier open.
      cwd = recentCwds[0]
    }
    await tick()
    firstFieldEl?.focus()
  })

  function onLauncherKeydown(e: KeyboardEvent) {
    if (e.key !== "Escape") return
    if (showBrowser) closeBrowser()
    else onClose()
  }

  function openBrowser() {
    showBrowser = true
  }

  function closeBrowser() {
    showBrowser = false
  }

  function useBrowsedFolder(path: string) {
    cwd = path
    showBrowser = false
  }

  function onLauncherSubmit(e: SubmitEvent) {
    e.preventDefault()
    void launch()
  }

  async function launch() {
    launching = true
    launchError = null
    try {
      // Only claude actually understands `--resume`; the field itself is
      // hidden for any other preset, but the trim-and-check happens here
      // too so a stale value left over from switching presets mid-launcher
      // session can never leak into an unrelated command's argv.
      const resumeId = selectedPreset === "claude" ? resumeSessionId.trim() : ""
      const body: CreateSessionRequest = selectedPreset
        ? {
            kind: "agent",
            preset: selectedPreset,
            cwd: cwd || homeDir || "/",
            cols: 120,
            rows: 36,
            ...(resumeId ? { args: ["--resume", resumeId] } : {}),
          }
        : { kind: "shell", command: customCommand, cwd: cwd || homeDir || "/", cols: 120, rows: 36 }
      await onLaunch(body)
    } catch (e) {
      launchError = describeError(e)
    } finally {
      launching = false
    }
  }
</script>

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
        placeholder={homeDir ? `${homeDir}/project` : "/path/to/project"}
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
    <DirectoryBrowser initialPath={cwd || null} onSelect={useBrowsedFolder} onClose={closeBrowser} />
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
    <button type="button" class="btn" onclick={onClose} disabled={launching}>Cancel</button>
    <button type="submit" class="btn btn--primary" disabled={launching}>
      {launching ? "Launching…" : "Launch"}
    </button>
  </div>
</form>

<style>
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

  /* Block: cwd-chip -- a tap-to-fill recent working directory (sibling of
     launcher, not launcher__recent__chip: BEM elements don't nest). */
  .cwd-chip {
    background: var(--surface);
    color: var(--muted);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    padding: 0.3rem 0.55rem;
    font-size: 0.78rem;
    font-family: var(--font-mono);
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
</style>
