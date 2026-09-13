<script lang="ts">
  // A status dot with its text twin (UI.md rule 18): the dot is aria-hidden,
  // the label is either visible beside it or screen-reader-only. Knows no
  // domain -- the caller maps its state to a tone.
  export type DotTone = "success" | "warning" | "warning-strong" | null

  let {
    tone = null,
    pulse = false,
    label,
    showLabel = false,
  }: {
    tone?: DotTone
    pulse?: boolean
    label: string
    showLabel?: boolean
  } = $props()
</script>

<span class="status-dot">
  <span
    class="dot"
    aria-hidden="true"
    class:dot--success={tone === "success"}
    class:dot--warning={tone === "warning"}
    class:dot--warning-strong={tone === "warning-strong"}
    class:dot--pulse={pulse}
  ></span>
  {#if showLabel}
    <span class="status-dot__label">{label}</span>
  {:else}
    <span class="sr-only">{label}.</span>
  {/if}
</span>

<style>
  /* Block: status-dot -- the .dot (app.css) plus its label. */
  .status-dot {
    display: inline-flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
  }
  .status-dot__label {
    font-size: 0.75rem;
    opacity: 0.7;
  }
</style>
