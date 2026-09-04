<script lang="ts">
  // Routing between list and session views (docs/09-frontend.md#structure).
  // No router library -- a hash suffices for two view types and keeps
  // "no state-management library" (docs/09-frontend.md#explicitly-not-in-the-frontend).
  import { onDestroy, onMount } from "svelte";
  import Sessions from "./lib/Sessions.svelte";
  import Session from "./lib/Session.svelte";

  let hash = $state(window.location.hash);

  function onHashChange() {
    hash = window.location.hash;
  }

  onMount(() => window.addEventListener("hashchange", onHashChange));
  onDestroy(() => window.removeEventListener("hashchange", onHashChange));

  let sessionId = $derived(hash.match(/^#\/sessions\/(.+)$/)?.[1] ?? null);

  function openSession(id: string) {
    window.location.hash = `#/sessions/${id}`;
  }

  function backToList() {
    window.location.hash = "#/";
  }
</script>

{#if sessionId}
  <Session sessionId={sessionId} onBack={backToList} />
{:else}
  <Sessions onOpen={openSession} />
{/if}
