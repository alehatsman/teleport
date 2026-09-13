<script lang="ts">
  import { onMount } from "svelte"
  import { browse, describeError } from "@/api/api"
  import type { BrowseEntry } from "@/api/types"
  import ErrorBanner from "@/ui/ErrorBanner.svelte"

  // Inline GET /api/v1/browse directory picker, opened from
  // SessionLauncher's "Browse…" button. No keydown handler of its own --
  // it renders inside the launcher's own <form>, so Escape already bubbles
  // up to SessionLauncher's onLauncherKeydown, which checks its own
  // showBrowser state first.

  let {
    initialPath,
    onSelect,
    onClose,
  }: {
    initialPath: string | null
    onSelect: (path: string) => void
    onClose: () => void
  } = $props()

  let browsePath: string | null = $state(null)
  let browseParent: string | null = $state(null)
  let browseEntries: BrowseEntry[] = $state([])
  let browseError: string | null = $state(null)
  let browseLoading = $state(false)

  async function loadBrowse(path?: string) {
    browseLoading = true
    browseError = null
    try {
      const res = await browse(path)
      browsePath = res.path
      browseParent = res.parent
      browseEntries = res.entries
    } catch (e) {
      browseError = describeError(e)
    } finally {
      browseLoading = false
    }
  }

  onMount(() => {
    // Start from whatever was already typed in the launcher -- browsing is
    // for refining a starting point (recent cwd, hand-typed guess), not
    // always starting over from home. loadBrowse() itself falls back to the
    // daemon's home directory when given nothing.
    void loadBrowse(initialPath ?? undefined)
  })
</script>

<div class="browser">
  <div class="browser__path">
    {#if browseLoading}
      Loading…
    {:else}
      {browsePath ?? "…"}
    {/if}
  </div>
  {#if browseError}
    <ErrorBanner message={browseError} />
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
  <div class="browser__actions">
    <button type="button" class="btn" onclick={onClose}>Cancel</button>
    <button type="button" class="btn btn--primary" disabled={!browsePath} onclick={() => browsePath && onSelect(browsePath)}>
      Use this folder
    </button>
  </div>
</div>

<style>
  /* Block: browser -- the inline GET /api/v1/browse directory picker.
     Sibling of launcher, not nested under it (it's a whole separate panel,
     not one of launcher's own fields), the same reasoning session-row is
     its own block rather than session-list's. */
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
    font-family: var(--font-mono);
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
    font-family: var(--font-mono);
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
  /* Same shape as SessionLauncher's .launcher__actions -- Svelte scopes
     styles per component, so a shared class name wouldn't reach markup
     rendered here. Three properties; duplicating them is cheaper and more
     local than promoting to app.css for one second consumer. */
  .browser__actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }
</style>
