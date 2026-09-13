<script lang="ts">
  import type { Session } from "@/api/types"
  import SessionRow from "@/features/sessions/SessionRow.svelte"

  let {
    sessions,
    now,
    homeDir,
    onOpen,
    onResume,
    onTerminate,
    onPurge,
  }: {
    sessions: Session[]
    now: number
    homeDir: string | null
    onOpen: (id: string) => void
    onResume: (session: Session) => void
    onTerminate: (id: string) => void
    onPurge: (id: string) => void
  } = $props()

  // Swipe-reveal exclusivity: only one row's action button is ever revealed
  // at a time. Owned here, not in SessionRow, because opening one row must
  // close whichever other row was open -- a concern about the list, not
  // about any single row.
  let openRowId: string | null = $state(null)
</script>

<ul class="session-list">
  {#each sessions as session (session.id)}
    <SessionRow
      {session}
      {now}
      {homeDir}
      isOpen={openRowId === session.id}
      onOpenChange={(open) => (openRowId = open ? session.id : null)}
      {onOpen}
      {onResume}
      {onTerminate}
      {onPurge}
    />
  {/each}
</ul>

<style>
  /* Block: session-list -- just the list container; each item is its own
     block (session-row, in its own component) since it has too many parts
     to stay one element deep. */
  .session-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
</style>
