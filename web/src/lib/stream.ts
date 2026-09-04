// The offset contract (docs/04-api-protocol.md#offsets-are-the-replay-index),
// owned entirely by this file per docs/09-frontend.md#streamts--the-part-that-must-be-right.
// `Terminal.svelte` and `Session.svelte` deal in these callbacks; neither
// touches a `WebSocket` directly.

import { streamUrl } from "./api";
import { CLIENT_ID, CLIENT_NAME, getToken } from "./identity";
import type { ClientMessage, ErrorFrame, ServerFrame, StreamState } from "./types";

const DESKTOP_TAIL = 1024 * 1024; // 1 MiB -- matches the daemon's own default_tail
const MOBILE_TAIL = 256 * 1024; // docs/15-open-questions.md#n3: a phone should ask for less
const MOBILE_VIEWPORT_PX = 700;
const BACKOFF_MIN_MS = 250;
const BACKOFF_MAX_MS = 8000;

/** N3: the client picks `tail`, not the daemon default, per viewport. */
function initialTail(): number {
  return window.innerWidth < MOBILE_VIEWPORT_PX ? MOBILE_TAIL : DESKTOP_TAIL;
}

export interface SessionStreamCallbacks {
  onState(state: StreamState): void;
  /** Raw PTY output, in order. Write straight to xterm -- never decode to a string. */
  onOutput(bytes: Uint8Array): void;
  /** `ready`'s or `resized`'s geometry. Observers letterbox to this; never fit it to their own viewport. */
  onGeometry(cols: number, rows: number): void;
  onControlChange(hasControl: boolean, controllerName: string | null): void;
  /** Replay started mid-VT-stream; caller must `term.reset()` before the next `onOutput`. */
  onTruncated(): void;
  onExit(code: number | null, finalOffset: number): void;
  onError(code: string, message: string | undefined): void;
}

/**
 * One WebSocket per attached session. Owns the offset cursor, the control
 * lease's *intent* (as opposed to its granted state), and jittered
 * reconnect. See docs/09-frontend.md#streamts--the-part-that-must-be-right --
 * this is a close translation of that file's pseudocode into a real client.
 */
export class SessionStream {
  private ws: WebSocket | null = null;
  private nextOffset = 0;
  private hasCursor = false;
  private wantControl: boolean;
  private backoff = BACKOFF_MIN_MS;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closedByCaller = false;
  private sawExit = false;

  constructor(
    private readonly sessionId: string,
    private readonly callbacks: SessionStreamCallbacks,
    opts?: { requestControl?: boolean },
  ) {
    this.wantControl = opts?.requestControl ?? false;
  }

  connect(): void {
    if (this.closedByCaller) return;
    this.callbacks.onState(this.hasCursor ? "reconnecting" : "connecting");

    const query = new URLSearchParams();
    if (this.hasCursor) {
      query.set("after", String(this.nextOffset));
    } else {
      query.set("tail", String(initialTail()));
    }
    // `mode=control` asks to *resume* a lease; it never preempts -- safe on
    // every reconnect, including the very first connect if the caller asked
    // for control up front.
    query.set("mode", this.wantControl ? "control" : "observe");
    query.set("client_id", CLIENT_ID);
    query.set("client_name", CLIENT_NAME);
    const token = getToken();
    if (token) query.set("token", token);

    const ws = new WebSocket(streamUrl(this.sessionId, query));
    ws.binaryType = "arraybuffer";
    this.ws = ws;

    ws.onopen = () => {
      this.callbacks.onState("replaying");
    };
    ws.onmessage = (event) => this.onMessage(event);
    ws.onclose = () => this.onClose();
    ws.onerror = () => {
      // The close event that follows carries the real signal; nothing
      // actionable here beyond what onclose already does.
    };
  }

  /** Explicit teardown -- component unmount. No further reconnects. */
  disconnect(): void {
    this.closedByCaller = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.ws?.close(1000, "client disconnect");
    this.ws = null;
  }

  /** Explicit user action only -- the one call that preempts (docs/09-frontend.md#control-lease-ui). */
  takeControl(): void {
    this.wantControl = true;
    this.send({ type: "claim_control" });
  }

  releaseControl(): void {
    this.wantControl = false;
    this.send({ type: "release_control" });
  }

  /** Only meaningful, and only sent, when this client is the controller. Callers gate that; this just forwards. */
  sendResize(cols: number, rows: number): void {
    this.send({ type: "resize", cols, rows });
  }

  /** Raw input bytes -- no framing, forwarded verbatim to the PTY writer. */
  sendInput(data: Uint8Array | string): void {
    if (this.ws?.readyState !== WebSocket.OPEN) return;
    this.ws.send(data as BufferSource | string);
  }

  private send(message: ClientMessage): void {
    if (this.ws?.readyState !== WebSocket.OPEN) return;
    this.ws.send(JSON.stringify(message));
  }

  private onMessage(event: MessageEvent): void {
    if (typeof event.data === "string") {
      this.onControlFrame(JSON.parse(event.data) as ServerFrame);
      return;
    }
    this.onBinaryFrame(event.data as ArrayBuffer);
  }

  private onControlFrame(frame: ServerFrame): void {
    switch (frame.type) {
      case "ready": {
        this.nextOffset = frame.replay_from;
        this.hasCursor = true;
        this.backoff = BACKOFF_MIN_MS; // a successful attach is the signal a flaky link recovered
        this.callbacks.onGeometry(frame.cols, frame.rows);
        this.callbacks.onControlChange(frame.control, frame.controller);
        if (frame.truncated) this.callbacks.onTruncated();
        this.callbacks.onState("live");
        return;
      }
      case "control_granted":
        this.callbacks.onControlChange(true, CLIENT_NAME);
        return;
      case "control_revoked":
        // `wantControl` is untouched: it's user intent, not connection
        // state (docs/09-frontend.md#streamts). A future reconnect may
        // still ask to resume -- the server never preempts on `mode=control`,
        // so that's always safe, it just won't get granted here.
        this.callbacks.onControlChange(false, frame.to);
        return;
      case "resized":
        this.callbacks.onGeometry(frame.cols, frame.rows);
        return;
      case "exit":
        this.sawExit = true;
        this.callbacks.onExit(frame.code, frame.final_offset);
        return;
      case "error":
        this.onErrorFrame(frame);
        return;
    }
  }

  private onErrorFrame(frame: ErrorFrame): void {
    if (frame.code === "offset_ahead") {
      // The one case where restarting at the default tail instead of our
      // tracked cursor is correct -- our offset is stale past what the
      // daemon can still serve. The server closes the socket right after
      // this; onclose's reconnect will pick up the dropped cursor.
      this.hasCursor = false;
    }
    this.callbacks.onError(frame.code, frame.message);
  }

  private onBinaryFrame(buf: ArrayBuffer): void {
    const view = new DataView(buf);
    const offset = Number(view.getBigUint64(0, false)); // big-endian
    const payload = new Uint8Array(buf, 8);

    if (offset < this.nextOffset) return; // already seen; drop
    this.nextOffset = offset + payload.length;
    this.callbacks.onOutput(payload);
  }

  private onClose(): void {
    this.ws = null;
    if (this.closedByCaller) {
      this.callbacks.onState("closed");
      return;
    }
    if (this.sawExit) {
      // The session is done; reconnecting would just replay the same tail
      // forever against a process that no longer exists.
      this.callbacks.onState("closed");
      return;
    }
    // Reconnection is normal, not an error (docs/09-frontend.md#connection-status).
    // 1013 (slow_consumer) included -- an expected event, reconnect exactly
    // like any other drop.
    this.callbacks.onState("reconnecting");
    const jitter = Math.random() * this.backoff * 0.25;
    this.reconnectTimer = setTimeout(() => this.connect(), this.backoff + jitter);
    this.backoff = Math.min(this.backoff * 2, BACKOFF_MAX_MS);
  }
}
