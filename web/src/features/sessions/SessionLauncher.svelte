<script lang="ts">
  import { onMount, tick } from "svelte"
  import { describeError } from "@/api/api"
  import type { CreateSessionRequest, Preset, Session } from "@/api/types"
  import LocationPicker from "@/features/sessions/LocationPicker.svelte"
  import ErrorBanner from "@/ui/ErrorBanner.svelte"
  import { buildLaunchRequest } from "./launchRequest"
  import { LOCATION_CHIPS_MAX, type Location } from "./locations"

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
    locations,
    homeDir,
    resumeSession,
    onTogglePin,
    onLaunch,
    onClose,
  }: {
    cwd: string
    selectedPreset: string
    customCommand: string
    presets: Preset[]
    locations: Location[]
    homeDir: string | null
    resumeSession: Session | null
    onTogglePin: (path: string, pinned: boolean) => void
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
  // True when the launcher was opened by "Resume" on a closed session.
  // Kept apart from `resumeSessionId` being non-empty: a restore with no
  // known id still resumes -- via Claude Code's own picker -- and that is
  // the common case now (see launchRequest.ts).
  let resumeRequested = $derived(resumeSession !== null)
  let firstFieldEl: HTMLSelectElement | undefined = $state()
  let showPicker = $state(false)
  // The inline shortlist. The rest of `locations` stays reachable through
  // the datalist and the browser; a chip wall taller than the form is not a
  // shortlist (docs/18-locations.md#stage-1--ranked-labelled-chips).
  let chipLocations: Location[] = $derived(locations.slice(0, LOCATION_CHIPS_MAX))

  onMount(async () => {
    if (resumeSession) {
      // "Resume this" on a closed session -- set up to continue that exact
      // conversation instead of making the id be found, copied, and pasted
      // in by hand.
      selectedPreset = "claude"
      resumeSessionId = resumeSession.claude_resume_id ?? ""
      cwd = resumeSession.cwd
    } else if (!cwd && locations[0] !== undefined) {
      // Prefill with the best-ranked directory -- typing the same path every
      // launch is the friction this is meant to remove. Only when empty:
      // never clobber whatever was carried over from an earlier open.
      cwd = locations[0].path
    }
    await tick()
    firstFieldEl?.focus()
  })

  function onLauncherKeydown(e: KeyboardEvent) {
    if (e.key !== "Escape") return
    // Innermost panel first: Escape in the picker closes the picker, not the
    // whole form the user was halfway through filling in.
    if (showPicker) showPicker = false
    else onClose()
  }

  function usePickedLocation(path: string) {
    cwd = path
    showPicker = false
  }

  function onLauncherSubmit(e: SubmitEvent) {
    e.preventDefault()
    void launch()
  }

  async function launch() {
    launching = true
    launchError = null
    try {
      const body = buildLaunchRequest({
        selectedPreset,
        customCommand,
        cwd,
        homeDir,
        resumeSessionId,
        resumeRequested,
      })
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
      <input
        type="text"
        bind:value={customCommand}
        autocapitalize="none"
        autocorrect="off"
        spellcheck="false"
        required
      />
    </label>
  {/if}
  {#if selectedPreset === "claude"}
    <label class="launcher__field">
      Resume session ID (optional)
      {#if resumeRequested}
        <!-- A restore with no id in hand is the normal case: Claude Code
             stopped emitting the link this is read from. Say what Launch
             will actually do rather than leaving a blank field looking
             like a failure. -->
        <span class="launcher__hint">
          {resumeSessionId
            ? "Continues this exact conversation."
            : "Blank — Claude Code will list this folder's conversations to pick from."}
        </span>
      {/if}
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
      <button type="button" class="btn launcher__browse-btn" onclick={() => (showPicker = true)}>
        Choose…
      </button>
    </div>
    {#if locations.length > 0}
      <!-- Full paths here, not the chips' basename+parent split: this one is
           a typing aid, and a label you cannot type into the field is no use
           in it. -->
      <datalist id="recent-cwds">
        {#each locations as loc (loc.path)}
          <option value={loc.path}></option>
        {/each}
      </datalist>
    {/if}
  </label>
  {#if showPicker}
    <LocationPicker
      value={cwd}
      {locations}
      {onTogglePin}
      onSelect={usePickedLocation}
      onClose={() => (showPicker = false)}
    />
  {/if}
  {#if chipLocations.length > 0}
    <!-- datalist above covers typing; these are for tapping -- a
         datalist's dropdown affordance is inconsistent on mobile
         (docs/09-frontend.md#mobile), and re-typing a path you've
         already used is exactly the friction this removes. Basename first,
         parent dimmed behind it: eight full paths under one home directory
         differ only in the part the ellipsis eats
         (docs/18-locations.md#what-is-wrong-today). -->
    <div class="launcher__recent">
      {#each chipLocations as loc (loc.path)}
        <button
          type="button"
          class="chip launcher__chip"
          class:chip--active={loc.path === cwd}
          title={loc.path}
          onclick={() => (cwd = loc.path)}
        >
          <span class="launcher__chip-name">{loc.name}</span>
          {#if loc.parent}
            <span class="launcher__chip-parent">{loc.parent}</span>
          {/if}
        </button>
      {/each}
    </div>
  {/if}
  {#if launchError}
    <ErrorBanner message={launchError} />
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
  /* Sits between the label text and the input, so it reads as part of the
     label rather than as an error under the field. */
  .launcher__hint {
    font-size: 0.78rem;
    color: var(--muted);
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

  /* Element: launcher__chip -- what a location .chip (app.css) adds on top
     of the shared block: a path is monospace, and it is split into the
     basename and the directory above it. */
  .launcher__chip {
    display: flex;
    align-items: baseline;
    gap: 0.4rem;
    font-family: var(--font-mono);
    max-width: 100%;
    overflow: hidden;
  }
  .launcher__chip-name {
    color: var(--fg);
    flex-shrink: 0;
  }
  /* The parent is context, not identity: it dims, and it is the part that
     gets clipped when the chip runs out of room. */
  .launcher__chip-parent {
    color: var(--muted);
    font-size: 0.75rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
