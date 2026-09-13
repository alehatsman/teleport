<script lang="ts" module>
  export type StatusFilter = "active" | "closed"
</script>

<script lang="ts">
  // Status toggle + text search over the session list. Both values are
  // bindable and owned by Sessions.svelte, which does the filtering -- this
  // component only renders the controls and the counts it is handed.
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
</script>

<div class="session-filters">
  <div class="session-filters__toggle" role="tablist" aria-label="Filter by status">
    <button
      type="button"
      role="tab"
      aria-selected={statusFilter === "active"}
      class="chip"
      class:chip--active={statusFilter === "active"}
      onclick={() => (statusFilter = "active")}
    >
      Active ({activeCount})
    </button>
    <button
      type="button"
      role="tab"
      aria-selected={statusFilter === "closed"}
      class="chip"
      class:chip--active={statusFilter === "closed"}
      onclick={() => (statusFilter = "closed")}
    >
      Closed ({closedCount})
    </button>
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
  .session-filters__toggle {
    display: flex;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }
  .session-filters__search {
    display: block;
    width: 100%;
    margin-bottom: var(--space-3);
    font-size: 0.9rem;
  }
</style>
