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
  // width (docs/09-frontend.md#geometry).
  function letterbox() {
    if (!term?.element) return;
    const scaleX = wrapperEl.clientWidth / term.element.scrollWidth;
    const scaleY = wrapperEl.clientHeight / term.element.scrollHeight;
    const scale = Math.max(Math.min(scaleX, scaleY, 1), 0.1);
    containerEl.style.transformOrigin = "top left";
    containerEl.style.transform = `scale(${scale})`;
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

<div class="terminal-wrapper" bind:this={wrapperEl}>
  <div class="terminal-container" bind:this={containerEl}></div>
</div>

<style>
  .terminal-wrapper {
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: var(--surface-deep);
  }
</style>
