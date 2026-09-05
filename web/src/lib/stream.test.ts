// Tests for SessionStream: the WS-ticket handshake (#18), protocol-parser
// hardening against malformed frames (#19), and the replay/reconnect
// byte-offset invariant (#21) -- docs/09-frontend.md#streamts--the-part-that-must-be-right.
//
// No jsdom: `./api`'s `streamUrl`/`createWsTicket` are mocked outright, so
// nothing here ever touches `window.location`, and a `FakeWebSocket` below
// stands in for the real `WebSocket` global.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createWsTicket } from "./api";
import { SessionStream, type SessionStreamCallbacks } from "./stream";
import type { ReadyFrame, StreamState } from "./types";

vi.mock("./api", () => ({
  createWsTicket: vi.fn(),
  streamUrl: (id: string, query: URLSearchParams) => `ws://test/${id}?${query.toString()}`,
}));

// -- FakeWebSocket: the subset of the WebSocket API stream.ts actually uses --

class FakeWebSocket {
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  static instances: FakeWebSocket[] = [];

  readyState = 0;
  binaryType = "blob";
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  sent: unknown[] = [];
  closedWith: { code: number; reason: string } | null = null;

  constructor(public url: string) {
    FakeWebSocket.instances.push(this);
  }

  send(data: unknown): void {
    this.sent.push(data);
  }

  close(code = 1000, reason = ""): void {
    if (this.readyState === FakeWebSocket.CLOSED) return; // already closed; onclose fires once
    this.readyState = FakeWebSocket.CLOSED;
    this.closedWith = { code, reason };
    this.onclose?.();
  }

  // -- test-only driver methods, not part of the real WebSocket API --

  open(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.onopen?.();
  }

  receiveText(data: string): void {
    this.onmessage?.({ data });
  }

  receiveBinary(buf: ArrayBuffer): void {
    this.onmessage?.({ data: buf });
  }

  static latest(): FakeWebSocket {
    const ws = FakeWebSocket.instances.at(-1);
    if (!ws) throw new Error("no FakeWebSocket constructed yet");
    return ws;
  }

  static reset(): void {
    FakeWebSocket.instances = [];
  }
}

vi.stubGlobal("WebSocket", FakeWebSocket);
// `initialTail()` reads `window.innerWidth` to pick desktop vs. mobile tail
// size -- the only DOM global `stream.ts` touches. A real `window` (jsdom)
// would be overkill for one property; stub just this.
vi.stubGlobal("window", { innerWidth: 1024 });

/** Wire-format a binary frame: 8-byte big-endian offset prefix + payload. */
function encodeFrame(offset: number, payload: Uint8Array): ArrayBuffer {
  const buf = new ArrayBuffer(8 + payload.length);
  new DataView(buf).setBigUint64(0, BigInt(offset), false);
  new Uint8Array(buf, 8).set(payload);
  return buf;
}

function readyFrame(overrides: Partial<ReadyFrame> = {}): ReadyFrame {
  return {
    type: "ready",
    session_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
    replay_from: 0,
    next_offset: 0,
    truncated: false,
    log_capped_at: null,
    cols: 80,
    rows: 24,
    control: false,
    controller: null,
    ...overrides,
  };
}

/** Collects everything a SessionStream reports, for assertions. */
function harness() {
  const states: StreamState[] = [];
  const output: Uint8Array[] = [];
  const errors: Array<{ code: string; message: string | undefined }> = [];
  const callbacks = {
    onState: (s: StreamState) => states.push(s),
    onOutput: (b: Uint8Array) => output.push(b),
    onGeometry: () => {},
    onControlChange: () => {},
    onTruncated: () => {},
    onExit: () => {},
    onError: (code: string, message: string | undefined) => errors.push({ code, message }),
  };
  return { states, output, errors, callbacks };
}

function concat(chunks: Uint8Array[]): Uint8Array {
  const total = chunks.reduce((n, c) => n + c.length, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) {
    out.set(c, offset);
    offset += c.length;
  }
  return out;
}

// Every SessionStream a test constructs is tracked here and torn down in
// afterEach. Without this, a test that ends mid-backoff (a close/protocol
// violation that scheduled a reconnect the test never awaited) leaves a live
// timer running past that test -- it eventually fires for real, pushes a
// stray FakeWebSocket onto the *next* test's freshly-`reset()` shared
// instance list, and whichever test happens to be `await vi.waitFor`-polling
// for "instances.length > N" at that moment picks up the wrong socket.
const liveStreams: SessionStream[] = [];
function mkStream(sessionId: string, callbacks: SessionStreamCallbacks): SessionStream {
  const stream = new SessionStream(sessionId, callbacks);
  liveStreams.push(stream);
  return stream;
}

beforeEach(() => {
  FakeWebSocket.reset();
  vi.mocked(createWsTicket).mockReset();
  vi.mocked(createWsTicket).mockResolvedValue({ ticket: "tkt-1", expires_in: 30 });
});

afterEach(() => {
  for (const stream of liveStreams.splice(0)) stream.disconnect();
});

describe("ws-ticket handshake (#18)", () => {
  it("fetches a ticket before connecting and puts it, not a token, on the URL", async () => {
    const { callbacks } = harness();
    const stream = mkStream("s1", callbacks);

    stream.connect();
    await vi.waitFor(() => expect(FakeWebSocket.instances.length).toBeGreaterThan(0));

    expect(createWsTicket).toHaveBeenCalledWith("s1");
    const url = FakeWebSocket.latest().url;
    expect(url).toContain("ticket=tkt-1");
    expect(url).not.toContain("token=");
  });

  it("schedules a reconnect, not a fall-back to a token, when the ticket fetch fails", async () => {
    vi.useFakeTimers();
    try {
      vi.mocked(createWsTicket).mockRejectedValueOnce(new Error("network"));
      vi.mocked(createWsTicket).mockResolvedValueOnce({ ticket: "tkt-2", expires_in: 30 });
      const { callbacks } = harness();
      const stream = mkStream("s1", callbacks);

      stream.connect();
      await vi.advanceTimersByTimeAsync(0); // flush the rejected fetch's microtask
      expect(FakeWebSocket.instances.length).toBe(0); // no socket opened on failure

      await vi.advanceTimersByTimeAsync(10_000); // past even the max backoff
      expect(FakeWebSocket.instances.length).toBe(1);
      expect(FakeWebSocket.latest().url).toContain("ticket=tkt-2");
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("protocol parser hardening (#19)", () => {
  async function connectedStream() {
    const h = harness();
    const stream = mkStream("s1", h.callbacks);
    stream.connect();
    await vi.waitFor(() => expect(FakeWebSocket.instances.length).toBeGreaterThan(0));
    const ws = FakeWebSocket.latest();
    ws.open();
    ws.receiveText(JSON.stringify(readyFrame()));
    return { stream, ws, ...h };
  }

  it("rejects a binary frame shorter than the 8-byte offset prefix", async () => {
    const { ws, output, errors } = await connectedStream();

    ws.receiveBinary(new ArrayBuffer(3));

    expect(output).toHaveLength(0);
    expect(errors).toHaveLength(1);
    expect(errors[0].code).toBe("protocol_violation");
    expect(ws.closedWith?.code).toBe(1002);
  });

  it("a short frame does not corrupt the offset cursor the next reconnect sends", async () => {
    vi.useFakeTimers();
    try {
      const { ws } = await connectedStream();
      ws.receiveBinary(encodeFrame(0, new Uint8Array([1, 2, 3, 4]))); // nextOffset -> 4
      ws.receiveBinary(new ArrayBuffer(3)); // protocol violation -- closes this socket

      // onClose sees a non-caller close with a cursor already established,
      // so it reconnects on the normal jittered-backoff path.
      await vi.advanceTimersByTimeAsync(10_000); // past even the max backoff
      expect(FakeWebSocket.instances.length).toBe(2);
      // If the malformed frame had corrupted nextOffset (e.g. left it at 0,
      // or thrown before this bookkeeping ran), the reconnect would ask for
      // the wrong replay window here.
      expect(FakeWebSocket.latest().url).toContain("after=4");
    } finally {
      vi.useRealTimers();
    }
  });

  it("treats a malformed JSON control frame as a protocol violation, not a thrown exception", async () => {
    const { ws, errors } = await connectedStream();

    expect(() => ws.receiveText("{not json")).not.toThrow();

    expect(errors).toHaveLength(1);
    expect(errors[0].code).toBe("protocol_violation");
    expect(ws.closedWith?.code).toBe(1002);
  });
});

describe("replay/reconnect byte-offset invariant (#21)", () => {
  it("drops an exact duplicate frame without advancing the cursor or emitting output twice", async () => {
    const h = harness();
    const stream = mkStream("s1", h.callbacks);
    stream.connect();
    await vi.waitFor(() => expect(FakeWebSocket.instances.length).toBeGreaterThan(0));
    const ws = FakeWebSocket.latest();
    ws.open();
    ws.receiveText(JSON.stringify(readyFrame()));

    const payload = new Uint8Array([10, 20, 30]);
    ws.receiveBinary(encodeFrame(0, payload));
    ws.receiveBinary(encodeFrame(0, payload)); // exact duplicate -- catch-up resending the same round

    expect(h.output).toHaveLength(1);
    expect(Array.from(concat(h.output))).toEqual([10, 20, 30]);
  });

  it("a reconnect's ready.replay_from matching the client's own cursor produces no gap across the boundary", async () => {
    // `ws.rs`'s `bound_attach` only ever clamps `after` *forward* (never
    // behind what the client asked for), so a compliant reconnect's
    // `replay_from` is always >= the cursor `stream.ts` itself sent as
    // `after=` -- this exercises that real boundary, not an adversarial one.
    const h = harness();
    const stream = mkStream("s1", h.callbacks);

    stream.connect();
    await vi.waitFor(() => expect(FakeWebSocket.instances.length).toBeGreaterThan(0));
    let ws = FakeWebSocket.latest();
    ws.open();
    ws.receiveText(JSON.stringify(readyFrame({ replay_from: 0 })));
    ws.receiveBinary(encodeFrame(0, new Uint8Array([1, 2, 3, 4])));

    // Connection drops mid-stream (not a clean exit) -- stream.ts reconnects on its own.
    ws.close(1006, "abnormal");
    await vi.waitFor(() => expect(FakeWebSocket.instances.length).toBeGreaterThan(1));
    ws = FakeWebSocket.latest();
    expect(ws.url).toContain("after=4"); // the cursor it actually asked for
    ws.open();
    ws.receiveText(JSON.stringify(readyFrame({ replay_from: 4 }))); // honored exactly
    ws.receiveBinary(encodeFrame(4, new Uint8Array([5, 6])));

    expect(Array.from(concat(h.output))).toEqual([1, 2, 3, 4, 5, 6]);
  });

  it("soak: a randomized sequence of contiguous chunks plus replayed overlaps reconstructs exactly", async () => {
    // Deterministic PRNG (mulberry32) -- no new dependency, reproducible failures.
    function mulberry32(seed: number) {
      let a = seed;
      return () => {
        a |= 0;
        a = (a + 0x6d2b79f5) | 0;
        let t = Math.imul(a ^ (a >>> 15), 1 | a);
        t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
      };
    }
    const rand = mulberry32(0xc0ffee);

    const source = new Uint8Array(2000);
    for (let i = 0; i < source.length; i++) source[i] = Math.floor(rand() * 256);

    // Chunk the source contiguously, then interleave a few replayed
    // (already-delivered) ranges -- exactly what a reconnect's catch-up
    // resend looks like on the wire.
    const chunks: Array<{ offset: number; bytes: Uint8Array }> = [];
    let pos = 0;
    while (pos < source.length) {
      const size = 1 + Math.floor(rand() * 50);
      const end = Math.min(pos + size, source.length);
      chunks.push({ offset: pos, bytes: source.slice(pos, end) });
      pos = end;
    }
    // Real chunk granularity: the daemon's fanout tags each chunk with the
    // offset of its own first byte (session/types.rs's `Chunk`) and a
    // catch-up resend only ever re-sends one of those whole chunks verbatim
    // -- never an arbitrary sub-slice straddling old and new bytes.
    const withReplays: typeof chunks = [];
    const delivered: (typeof chunks)[number][] = [];
    for (const chunk of chunks) {
      if (delivered.length > 0 && rand() < 0.2) {
        withReplays.push(delivered[Math.floor(rand() * delivered.length)]);
      }
      withReplays.push(chunk);
      delivered.push(chunk);
    }

    const h = harness();
    const stream = mkStream("s1", h.callbacks);
    stream.connect();
    await vi.waitFor(() => expect(FakeWebSocket.instances.length).toBeGreaterThan(0));
    const ws = FakeWebSocket.latest();
    ws.open();
    ws.receiveText(JSON.stringify(readyFrame({ replay_from: 0 })));
    for (const { offset, bytes } of withReplays) {
      ws.receiveBinary(encodeFrame(offset, bytes));
    }

    expect(Array.from(concat(h.output))).toEqual(Array.from(source));
  });
});
