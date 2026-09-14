<script lang="ts">
  // Status toggle + text search over the session list. Both values are
  // bindable and owned by Sessions.svelte, which does the filtering -- this
  // component only renders the controls and the counts it is handed.
  import type { StatusFilter } from "./sessionDisplay"

  let {
    statusFilter = $bindable(),
    searchQuery = $bindable(),
    activeCount,
    closedCount,
  }: {
    statusFilter: StatusFilter
    searchQuery: string
    activeCount: number
    closedCount: number
  } = $props()

  let tabs: { value: StatusFilter; label: string }[] = $derived([
    { value: "active", label: `Active (${activeCount})` },
    { value: "closed", label: `Closed (${closedCount})` },
  ])
</script>

<div class="session-filters">
  <div class="session-filters__toggle" role="tablist" aria-label="Filter by status">
    {#each tabs as tab (tab.value)}
      <button
        type="button"
        role="tab"
        aria-selected={statusFilter === tab.value}
        class="chip"
        class:chip--active={statusFilter === tab.value}
        onclick={() => (statusFilter = tab.value)}
      >
        {tab.label}
      </button>
    {/each}
  </div>
  <input
    type="search"
    class="session-filters__search"
    placeholder="Search sessions…"
    aria-label="Search sessions"
    bind:value={searchQuery}
    autocapitalize="none"
    autocorrect="off"
    spellcheck="false"
  />
</div>

<style>
  /* Block: session-filters -- the controls above the list. */
  .session-filters {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin-bottom: var(--space-3);
  }
  .session-filters__toggle {
    display: flex;
    gap: var(--space-2);
  }
  .session-filters__search {
    width: 100%;
  }
</style>
