<script lang="ts">
  import { onMount, tick } from "svelte"
  import DirectoryBrowser from "@/features/sessions/DirectoryBrowser.svelte"
  import { type Location, matchLocations } from "./locations"

  // The full location list, searchable (docs/18-locations.md#stage-2--the-location-picker).
  // The launcher's chips are this list's top few hoisted inline; everything
  // past them is only reachable here. `Browse filesystem…` at the bottom
  // swaps in the existing DirectoryBrowser for a directory teleport has
  // never launched in and nobody has pinned.
  let {
    value,
    locations,
    onSelect,
    onTogglePin,
    onClose,
  }: {
    /** The cwd currently in the field, marked as selected in the list. */
    value: string
    locations: Location[]
    onSelect: (path: string) => void
    onTogglePin: (path: string, pinned: boolean) => void
    onClose: () => void
  } = $props()

  let query = $state("")
  let results: Location[] = $derived(matchLocations(locations, query))
  let showBrowser = $state(false)
  let searchEl: HTMLInputElement | undefined = $state()

  onMount(async () => {
    // Fine pointers only. On a phone, autofocus raises the soft keyboard over
    // the list the user opened this to read -- and the list is already
    // ranked, so the top few taps are the common case, not typing.
    if (!window.matchMedia("(pointer: coarse)").matches) {
      await tick()
      searchEl?.focus()
    }
  })

  function onSearchKeydown(e: KeyboardEvent) {
    if (e.key !== "Enter") return
    // This renders inside the launcher's <form>, where Enter in a text field
    // submits -- i.e. launches. Take the top result instead, which is what
    // Enter in a search box means everywhere else.
    e.preventDefault()
    const first = results[0]
    if (first) onSelect(first.path)
  }
</script>

<div class="picker">
  {#if showBrowser}
    <DirectoryBrowser
      initialPath={value || null}
      onSelect={(path) => onSelect(path)}
      onClose={() => (showBrowser = false)}
    />
  {:else}
    <input
      type="search"
      class="picker__search"
      placeholder="Search locations…"
      aria-label="Search locations"
      bind:value={query}
      bind:this={searchEl}
      onkeydown={onSearchKeydown}
      autocapitalize="none"
      autocorrect="off"
      spellcheck="false"
    />
    <div class="picker__list">
      {#each results as loc (loc.path)}
        <div class="picker__row" class:picker__row--active={loc.path === value}>
          <button type="button" class="picker__pick" title={loc.path} onclick={() => onSelect(loc.path)}>
            <span class="picker__name">{loc.name}</span>
            {#if loc.parent}
              <span class="picker__parent">{loc.parent}</span>
            {/if}
          </button>
          <!-- The star lives on the row, not behind a settings screen: the
               moment you want a folder kept is the moment you are looking
               at it. -->
          <button
            type="button"
            class="picker__pin"
            class:picker__pin--on={loc.pinned}
            aria-pressed={loc.pinned}
            aria-label={loc.pinned ? `Unpin ${loc.path}` : `Pin ${loc.path}`}
            onclick={() => onTogglePin(loc.path, !loc.pinned)}
          >
            {loc.pinned ? "★" : "☆"}
          </button>
        </div>
      {:else}
        <p class="picker__empty">
          {locations.length === 0
            ? "No locations yet — browse for one."
            : "Nothing matches that."}
        </p>
      {/each}
    </div>
    <div class="picker__actions">
      <button type="button" class="btn" onclick={() => (showBrowser = true)}>Browse filesystem…</button>
      <button type="button" class="btn" onclick={onClose}>Cancel</button>
    </div>
  {/if}
</div>

<style>
  /* Block: picker -- the searchable location list. A sibling of launcher and
     browser, not a part of either: it is a whole panel with parts of its own
     (web/CLAUDE.md, UI.md rule 23). */
  .picker {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    background: var(--surface-deep);
  }
  .picker__search {
    width: 100%;
  }
  .picker__list {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    max-height: 40vh;
    overflow-y: auto;
  }
  .picker__row {
    /* Same reasoning as .browser__entry: flex items shrink by default, so
       without this the rows squash instead of the list scrolling. */
    flex: none;
    display: flex;
    align-items: stretch;
    gap: 0.2rem;
    border-radius: var(--radius-sm);
  }
  .picker__row:hover {
    background: var(--surface-hover);
  }
  .picker__row--active {
    background: var(--surface-hover);
  }
  .picker__pick {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    text-align: left;
    background: none;
    border: none;
    padding: 0.45rem 0.6rem;
    font-family: var(--font-mono);
    font-size: 0.85rem;
    color: var(--fg);
    cursor: pointer;
  }
  .picker__name {
    flex-shrink: 0;
  }
  /* Context, not identity -- dimmed, and the first thing clipped. */
  .picker__parent {
    color: var(--muted);
    font-size: 0.75rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .picker__pin {
    flex: none;
    background: none;
    border: none;
    padding: 0 0.6rem;
    font-size: 0.95rem;
    line-height: 1;
    color: var(--muted);
    cursor: pointer;
  }
  .picker__pin--on {
    color: var(--accent);
  }
  .picker__empty {
    margin: 0;
    padding: 0.45rem 0.6rem;
    color: var(--muted);
    font-size: 0.85rem;
  }
  /* Same three properties as .launcher__actions / .browser__actions, with
     the browse affordance pushed to the left edge; Svelte scopes styles per
     component, so these cannot share a class without promoting a block for
     it (see docs/09-frontend.md). */
  .picker__actions {
    display: flex;
    justify-content: space-between;
    gap: var(--space-2);
  }
</style>
