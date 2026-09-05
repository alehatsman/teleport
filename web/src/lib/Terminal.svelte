<script lang="ts">
  // The only file that imports xterm.js (docs/09-frontend.md#structure).
  // Everything else deals in session ids and connection state.
  import { onMount } from "svelte";
  import { Terminal as XTerm } from "@xterm/xterm";
  import { FitAddon } from "@xterm/addon-fit";
  import "@xterm/xterm/css/xterm.css";
  import type { SessionStream } from "./stream";

  let { stream, isController }: { stream: SessionStream; isController: boolean } = $props();

  let wrapperEl: HTMLDivElement;
  let containerEl: HTMLDivElement;
  let term: XTerm;
  let fitAddon: FitAddon;
  let resizeDebounce: ReturnType<typeof setTimeout> | null = null;
  let ptyCols = 80;
  let ptyRows = 24;
  // N3 (docs/15-open-questions.md#n3--xtermjs-write-pacing-on-reattach): a
  // replay round can be up to 1 MiB; writing that in one call stalls the
  // render thread right when the app is supposed to feel instant. Chaining
  // through `write`'s own callback serializes writes one paint apart instead
  // of firing them all synchronously back to back.
  let writeQueue: Promise<void> = Promise.resolve();

  onMount(() => {
    term = new XTerm({ scrollback: 10000, cursorBlink: true });
    fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(containerEl);

    // Only forwarded when this client holds the lease -- an observer's
    // keystrokes must never reach the PTY (docs/09-frontend.md#terminalsvelte).
    term.onData((data) => {
      if (isController) stream.sendInput(data);
    });

    const resizeObserver = new ResizeObserver(() => scheduleGeometryUpdate());
    resizeObserver.observe(wrapperEl);
    scheduleGeometryUpdate();

    return () => {
      resizeObserver.disconnect();
      if (resizeDebounce) clearTimeout(resizeDebounce);
      term.dispose();
    };
  });

  // Debounced 150ms per docs/09-frontend.md#terminalsvelte, for both the
  // controller's fit-to-viewport and the observer's letterbox recompute.
  function scheduleGeometryUpdate() {
    if (resizeDebounce) clearTimeout(resizeDebounce);
    resizeDebounce = setTimeout(applyGeometryPolicy, 150);
  }

  function applyGeometryPolicy() {
    if (!term) return;
    if (isController) {
      containerEl.style.transform = "";
      fitAddon.fit();
      stream.sendResize(term.cols, term.rows);
    } else {
      term.resize(ptyCols, ptyRows);
      letterbox();
    }
  }

  // Observers render the PTY's actual size, then scale the whole terminal
  // element to fit the viewport -- never re-wrap output for a different
  // width (docs/09-frontend.md#geometry). A desktop-sized PTY watched from a
  // phone shrinks a lot (scale can land well under 0.3) -- centering the
  // result is what makes that read as an intentional letterbox instead of a
  // terminal glued into the corner with the rest of the screen looking broken.
  function letterbox() {
    if (!term?.element) return;
    const scaleX = wrapperEl.clientWidth / term.element.scrollWidth;
    const scaleY = wrapperEl.clientHeight / term.element.scrollHeight;
    const scale = Math.max(Math.min(scaleX, scaleY, 1), 0.1);
    const offsetX = Math.max((wrapperEl.clientWidth - term.element.scrollWidth * scale) / 2, 0);
    const offsetY = Math.max((wrapperEl.clientHeight - term.element.scrollHeight * scale) / 2, 0);
    containerEl.style.transformOrigin = "top left";
    // translate() composes after scale() here, so the offset is in final
    // (post-scale) screen pixels -- exactly the centering slack computed above.
    containerEl.style.transform = `translate(${offsetX}px, ${offsetY}px) scale(${scale})`;
  }

  export function write(bytes: Uint8Array) {
    if (!term) return;
    const t = term;
    writeQueue = writeQueue.then(() => new Promise<void>((resolve) => t.write(bytes, resolve)));
  }

  export function reset() {
    // Routed through the same queue as `write` so it lands in order --
    // `onTruncated` fires right after `ready`, before any replay bytes, but
    // queuing keeps that true even if a caller's timing ever changes.
    if (!term) return;
    const t = term;
    writeQueue = writeQueue.then(() => {
      t.reset();
    });
  }

  /** `ready`'s or `resized`'s cols/rows -- the one PTY geometry every client renders. */
  export function setGeometry(cols: number, rows: number) {
    ptyCols = cols;
    ptyRows = rows;
    if (term && !isController) {
      term.resize(cols, rows);
      requestAnimationFrame(letterbox);
    }
  }

  // Re-apply the right policy the moment the lease changes hands, without
  // waiting for the next resize event.
  $effect(() => {
    isController; // dependency
    if (term) applyGeometryPolicy();
  });
</script>

<div class="terminal" bind:this={wrapperEl}>
  <div class="terminal__surface" bind:this={containerEl}></div>
</div>

<style>
  /* Block: terminal -- the xterm.js canvas frame (docs/09-frontend.md#terminalsvelte). */
  .terminal {
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: var(--surface-deep);
  }
  /* fitAddon.fit() measures *this* element (the one passed to term.open()),
     not .terminal -- without an explicit size here it only ever wraps its
     own content instead of reporting the space actually available, so the
     controller's fit-to-viewport silently settles for less than the full
     window (observed: real, empty space below the terminal even while
     controlling, not just the intentional observer letterbox). */
  .terminal__surface {
    width: 100%;
    height: 100%;
    /* xterm.js quantizes to whole rows/cols -- its own root element ends up
       a few pixels shorter/narrower than this container unless the fitted
       size happens to divide evenly (it essentially never does). That
       leftover is unavoidable, but center it instead of leaving it all at
       the bottom: a symmetric sliver top and bottom reads as deliberate
       framing, whereas one-sided leftover reads as the terminal drifting
       when a scrollbar draws attention to that edge (measured ~14px on
       this build, exact amount depends on font metrics/viewport size).
       clientWidth/clientHeight -- what fitAddon.fit() reads to decide
       rows/cols -- come from this flex container itself, not from its
       (centered) child, so this doesn't feed back into the row count. */
    display: flex;
    align-items: center;
    justify-content: center;
  }
</style>
