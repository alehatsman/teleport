//! A client for `anchord`'s control socket.
//!
//! This is the first half of running teleport's session layer on anchor
//! (anchor's `ROADMAP.md` M4, and its `docs/06-teleport-seam.md` for why the
//! seam sits here rather than under `SessionManager`). Nothing in teleport
//! uses it yet: the in-process path stays authoritative until the switch
//! lands behind a flag, default off.
//!
//! **Written against anchor's published protocol, not its crate.** Depending
//! on the `anchor` crate would couple the two release cycles, which is the
//! coupling the extraction exists to end — anchor's `docs/04-protocol.md` is
//! the contract, and a wire protocol is a contract a compiler cannot quietly
//! change under us.
//!
//! **One connection per request.** The protocol correlates by `id`, so a
//! single connection could carry many in flight, but that needs a
//! demultiplexer on top; a unix socket connection is cheap and
//! connection-per-call keeps failure isolated to the call that caused it.
//! An `attach` owns its connection for the whole stream regardless, because
//! the connection stops being request/response once it switches framing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use transport::{ReadHalf, WriteHalf};

/// The connection to anchord, one concrete type per platform behind one
/// pair of names.
///
/// **A type alias pair, not a trait**, which is the shape anchor's
/// `docs/09-windows.md` §3.1 proposes for its own side of the seam. A
/// `UnixStream` and a `NamedPipeClient` share no trait, but every use in
/// this file needs only "read", "write" and "drop to cancel", and
/// `AsyncRead`/`AsyncWrite` already say that. A trait object would buy an
/// abstraction nothing calls for and cost dynamic dispatch on the attach
/// path, which carries every byte of terminal output.
///
/// Everything platform-specific about reaching anchord lives in here, so
/// the protocol code above and below it is written once. The wire format
/// needs nothing from the transport but an ordered byte stream in each
/// direction ([anchor's `docs/04-protocol.md`]).
///
/// Cancellation is the one place the two arms could have differed and do
/// not. Anchor's own synchronous client needs a different mechanism on
/// Windows to unblock a read from another thread, because a named pipe has
/// no `shutdown` (its §4.1). This client is async: a blocked read is a
/// future, and dropping it cancels the read on either platform, so nothing
/// here has an `Interrupt` to port.
mod transport {
    #[cfg(unix)]
    pub(super) use self::unix::{connect, ReadHalf, WriteHalf};
    #[cfg(windows)]
    pub(super) use self::windows::{connect, ReadHalf, WriteHalf};

    #[cfg(unix)]
    mod unix {
        use std::io;
        use std::path::Path;

        /// The read half of a connection to anchord.
        pub(crate) type ReadHalf = tokio::net::unix::OwnedReadHalf;
        /// The write half of a connection to anchord.
        pub(crate) type WriteHalf = tokio::net::unix::OwnedWriteHalf;

        /// Connects to the `AF_UNIX` socket at `endpoint`. The socket's
        /// `0600` mode is the access check; nothing else is (anchor's
        /// `src/server.rs` module doc).
        pub(crate) async fn connect(endpoint: &Path) -> io::Result<(ReadHalf, WriteHalf)> {
            Ok(tokio::net::UnixStream::connect(endpoint)
                .await?
                .into_split())
        }
    }

    #[cfg(windows)]
    mod windows {
        use std::io;
        use std::path::Path;
        use std::time::Duration;

        use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

        /// The read half of a connection to anchord.
        ///
        /// `tokio::io::split` rather than an owned split: a named pipe
        /// client has no `into_split`. The halves share one handle behind a
        /// lock, which costs nothing here because each connection is driven
        /// by one task.
        pub(crate) type ReadHalf = tokio::io::ReadHalf<NamedPipeClient>;
        /// The write half of a connection to anchord.
        pub(crate) type WriteHalf = tokio::io::WriteHalf<NamedPipeClient>;

        /// `SECURITY_IDENTIFICATION` from `winnt.h`: `SecurityIdentification
        /// << 16`. Spelled out rather than imported because it is the one
        /// constant this arm needs and the value is fixed ABI.
        const SECURITY_IDENTIFICATION: u32 = 1 << 16;

        /// `ERROR_PIPE_BUSY`: every instance of the pipe is connected. The
        /// server has not yet created the next one, so a retry soon will
        /// usually find it.
        const ERROR_PIPE_BUSY: i32 = 231;

        /// How long a connect waits out `ERROR_PIPE_BUSY` before giving up.
        /// Bounded, because a server that never creates another instance is
        /// indistinguishable from a wedged one.
        const BUSY_BUDGET: Duration = Duration::from_secs(2);
        const BUSY_RETRY: Duration = Duration::from_millis(10);

        /// Connects to the named pipe at `endpoint`.
        ///
        /// At `SECURITY_IDENTIFICATION`, set explicitly rather than inherited
        /// from tokio's default, because it is a security property this
        /// client promises and a dependency's default is not a promise. A
        /// named-pipe server can impersonate whoever connects; at this level
        /// it can learn who connected but cannot act as them, so a squatted
        /// or compromised server gets no more than an identity (anchor's
        /// `docs/09-windows.md` §4.3).
        pub(crate) async fn connect(endpoint: &Path) -> io::Result<(ReadHalf, WriteHalf)> {
            let deadline = tokio::time::Instant::now() + BUSY_BUDGET;
            let pipe = loop {
                match ClientOptions::new()
                    .security_qos_flags(SECURITY_IDENTIFICATION)
                    .open(endpoint)
                {
                    Ok(pipe) => break pipe,
                    Err(e)
                        if e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                            && tokio::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(BUSY_RETRY).await;
                    }
                    Err(e) => return Err(e),
                }
            };
            Ok(tokio::io::split(pipe))
        }
    }
}

/// Frame tags, from anchor's `docs/04-protocol.md`.
mod frame {
    /// Output from the PTY.
    pub(super) const OUTPUT: u8 = 0x01;
    /// Input to the PTY. Lease holder only.
    pub(super) const INPUT: u8 = 0x02;
    /// The session exited.
    pub(super) const EXIT: u8 = 0x03;
    /// Lease state for this connection.
    pub(super) const LEASE: u8 = 0x04;
    /// Resize, client to daemon. Lease holder only.
    pub(super) const RESIZE: u8 = 0x05;
    /// Tag, offset, payload length.
    pub(super) const HEADER_LEN: usize = 1 + 8 + 4;
    /// The largest payload anchor will accept on an inbound frame.
    ///
    /// Anchor's, not ours: a `len` arriving from a client is an unverified
    /// claim it would allocate on, so it drops the connection on the header
    /// alone rather than trusting one. Exceeding this does not cost a frame,
    /// it costs the whole attach — an attached connection ends as soon as
    /// either direction does. See anchor's `docs/04-protocol.md`.
    pub(super) const MAX_INBOUND: usize = 64 * 1024;
}

/// How a session is spawned. Mirrors anchor's `spec` object.
#[derive(Debug, Clone, Serialize)]
pub struct AnchorSpec {
    /// The executable to spawn. Never a shell string.
    pub command: String,
    /// `command`'s argv, not including `command` itself.
    pub args: Vec<String>,
    /// Working directory to spawn in.
    pub cwd: PathBuf,
    /// Overrides layered onto the daemon's own environment.
    pub env: BTreeMap<String, String>,
    /// Initial PTY width.
    pub cols: u16,
    /// Initial PTY height.
    pub rows: u16,
    /// Run `command` through the user's login shell.
    pub login_shell: bool,
}

/// One session as anchor reports it.
///
/// `env` is deliberately absent: anchor does not echo a spec's environment
/// back, and teleport has no business asking it to.
#[derive(Debug, Clone, Deserialize)]
pub struct AnchorSession {
    /// The session's ULID.
    pub id: String,
    /// Caller-owned key/value.
    pub labels: BTreeMap<String, String>,
    /// `running`, `closing`, `exited` or `lost`.
    pub state: String,
    /// The child's OS pid, while it has one.
    pub pid: Option<u32>,
    /// The child's exit code, once it has one.
    pub exit_code: Option<i32>,
    /// Why the session is `lost`, if it is.
    pub lost_reason: Option<String>,
    /// Effective PTY width.
    pub cols: u16,
    /// Effective PTY height.
    pub rows: u16,
    /// The authoritative output offset.
    pub output_bytes: u64,
    /// Where persistence stopped, if it did.
    pub log_capped_at: Option<u64>,
    /// Milliseconds since the unix epoch.
    pub created_at: i64,
    /// When the session exited, if it has.
    pub exited_at: Option<i64>,
    /// The spawned executable.
    pub command: String,
    /// Its argv.
    pub args: Vec<String>,
    /// Its working directory.
    pub cwd: PathBuf,
    /// Whether it was wrapped in a login shell.
    pub login_shell: bool,
}

/// A range of a session's output log.
#[derive(Debug, Clone)]
pub struct AnchorLogRange {
    /// Offset of the first byte returned.
    pub from: u64,
    /// One past the last byte returned.
    pub to: u64,
    /// The session's authoritative offset at the time of the read.
    pub next_offset: u64,
    /// The bytes themselves. Raw PTY output, not UTF-8 in general.
    pub bytes: Vec<u8>,
}

/// A client for one `anchord` socket.
#[derive(Debug, Clone)]
pub struct AnchorClient {
    socket: PathBuf,
}

impl AnchorClient {
    /// A client for the daemon listening on `socket`. Connecting is deferred
    /// to the first call, so constructing one cannot fail.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The socket this client talks to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Spawns a session.
    pub async fn create(
        &self,
        spec: &AnchorSpec,
        labels: &BTreeMap<String, String>,
    ) -> Result<String> {
        let response = self
            .call("create", json!({ "spec": spec, "labels": labels }))
            .await?;
        response
            .get("session")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .context("create response carried no session id")
    }

    /// Every session matching `filter`, newest first. An empty filter lists
    /// everything.
    pub async fn list(&self, filter: &BTreeMap<String, String>) -> Result<Vec<AnchorSession>> {
        let response = self.call("list", json!({ "filter": filter })).await?;
        let sessions = response
            .get("sessions")
            .cloned()
            .context("list response carried no sessions")?;
        serde_json::from_value(sessions).context("decoding the session list")
    }

    /// One session by id, live or exited.
    pub async fn get(&self, id: &str) -> Result<AnchorSession> {
        let response = self.call("get", json!({ "session": id })).await?;
        let session = response
            .get("session")
            .cloned()
            .context("get response carried no session")?;
        serde_json::from_value(session).context("decoding the session")
    }

    /// Sets and removes labels, returning the resulting set.
    pub async fn label(
        &self,
        id: &str,
        set: &BTreeMap<String, String>,
        remove: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let response = self
            .call(
                "label",
                json!({ "session": id, "set": set, "remove": remove }),
            )
            .await?;
        let labels = response
            .get("labels")
            .cloned()
            .context("label response carried no labels")?;
        serde_json::from_value(labels).context("decoding the labels")
    }

    /// Applies a new PTY size.
    pub async fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        self.call(
            "resize",
            json!({ "session": id, "cols": cols, "rows": rows }),
        )
        .await
        .map(|_| ())
    }

    /// Terminates a session. Idempotent.
    pub async fn terminate(&self, id: &str) -> Result<()> {
        self.call("terminate", json!({ "session": id }))
            .await
            .map(|_| ())
    }

    /// Forgets a session entirely: its row, its labels and its output log.
    ///
    /// Anchor refuses this while the session is still running, so a caller
    /// terminates first. `terminate` then `purge` is the sequence teleport's
    /// `DELETE ?purge=true` maps onto.
    pub async fn purge(&self, id: &str) -> Result<()> {
        self.call("purge", json!({ "session": id }))
            .await
            .map(|_| ())
    }

    /// Reads a range of a session's output log.
    pub async fn log(&self, id: &str, from: u64, max: u64) -> Result<AnchorLogRange> {
        let response = self
            .call("log", json!({ "session": id, "from": from, "max": max }))
            .await?;
        let bytes = response
            .get("bytes")
            .and_then(Value::as_array)
            .context("log response carried no bytes")?
            .iter()
            .map(|b| {
                u8::try_from(b.as_u64().unwrap_or(u64::from(u8::MAX)) & 0xff).unwrap_or(u8::MAX)
            })
            .collect::<Vec<u8>>();
        Ok(AnchorLogRange {
            from: response.get("from").and_then(Value::as_u64).unwrap_or(from),
            to: response
                .get("to")
                .and_then(Value::as_u64)
                .unwrap_or(from + bytes.len() as u64),
            next_offset: response
                .get("next_offset")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            bytes,
        })
    }

    /// Attaches to a session's output stream from `from`.
    ///
    /// `control` requests the input lease. Anchor never preempts, so an
    /// attach that asks for it while another connection holds it still
    /// succeeds as an observer — check [`AnchorAttach::has_control`].
    pub async fn attach(&self, id: &str, from: u64, control: bool) -> Result<AnchorAttach> {
        let (read_half, mut write_half) = self.connect().await?;
        let mut reader = BufReader::new(read_half);

        let request = json!({
            "id": 1,
            "op": "attach",
            "session": id,
            "from": from,
            "control": control,
        });
        let response = exchange(&mut write_half, &mut reader, &request).await?;
        check_ok(&response)?;

        Ok(AnchorAttach {
            size: match (
                response.get("cols").and_then(Value::as_u64),
                response.get("rows").and_then(Value::as_u64),
            ) {
                // An older anchord does not send them; `None` means "ask
                // elsewhere", not "zero by zero".
                (Some(cols), Some(rows)) => u16::try_from(cols).ok().zip(u16::try_from(rows).ok()),
                _ => None,
            },
            has_control: response
                .get("control")
                .and_then(Value::as_bool)
                .unwrap_or_default(),
            next_offset: response
                .get("next_offset")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            reader,
            writer: write_half,
            pending: Vec::new(),
        })
    }

    /// Opens one connection to anchord, already split for reading and
    /// writing. The only place this file touches the transport.
    async fn connect(&self) -> Result<(ReadHalf, WriteHalf)> {
        transport::connect(&self.socket)
            .await
            .with_context(|| format!("connecting to {}", self.socket.display()))
    }

    /// One control request on its own connection.
    async fn call(&self, op: &str, body: Value) -> Result<Value> {
        let (read_half, mut write_half) = self.connect().await?;
        let mut reader = BufReader::new(read_half);

        // Built rather than indexed into: `Value`'s index panics on a
        // non-object, and every caller here passes an object literal.
        let mut request = serde_json::Map::new();
        request.insert("id".to_owned(), json!(1));
        request.insert("op".to_owned(), json!(op));
        if let Value::Object(fields) = body {
            request.extend(fields);
        }
        let response = exchange(&mut write_half, &mut reader, &Value::Object(request)).await?;
        check_ok(&response)?;
        Ok(response)
    }
}

/// A live attachment: the frames anchor sends, and input back to the PTY.
#[derive(Debug)]
pub struct AnchorAttach {
    size: Option<(u16, u16)>,
    has_control: bool,
    next_offset: u64,
    reader: BufReader<ReadHalf>,
    writer: WriteHalf,
    /// Bytes of the frame currently being read, kept here rather than on
    /// `next_frame`'s stack because `next_frame` is polled inside a
    /// `select!` and is dropped whenever another arm wins. With a local
    /// buffer those already-consumed bytes would be lost and the stream
    /// would resume mid-frame — which surfaces as a garbage header, then
    /// EOF, and a client told "the stream ended" instead of being handed
    /// the exit frame that was in flight.
    pending: Vec<u8>,
}

/// One frame from an attached session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorFrame {
    /// PTY output, tagged with the offset of its first byte.
    Output {
        /// Offset of the first byte.
        offset: u64,
        /// The bytes.
        bytes: Vec<u8>,
    },
    /// The session exited. `None` when the code is unknown.
    Exit {
        /// The child's exit code, if it could be observed.
        code: Option<i32>,
    },
    /// This connection's lease changed.
    Lease {
        /// Whether this connection now holds it.
        held: bool,
    },
    /// The PTY's size changed. Carries the size **actually applied** —
    /// after anchor's clamping and coalescing — not the size anyone asked
    /// for. Sent to every attached client including the one that asked, so
    /// a client that requested 5000 and got 1000 learns the truth rather
    /// than rendering its own request back.
    Resized {
        /// The width now in effect.
        cols: u16,
        /// The height now in effect.
        rows: u16,
    },
}

impl AnchorAttach {
    /// Whether this connection holds the input lease, as of the last
    /// `lease` frame seen.
    pub fn has_control(&self) -> bool {
        self.has_control
    }

    /// Records a lease change the caller read off the stream, so
    /// [`has_control`](Self::has_control) stays true to it.
    pub fn set_control(&mut self, held: bool) {
        self.has_control = held;
    }

    /// The session's authoritative offset when the attach was answered.
    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    /// The PTY's size when the attach was answered.
    ///
    /// Taken from the attach reply rather than a `get`, so a client starts
    /// from the real size instead of waiting for a `Resized` frame that a
    /// session nobody resizes will never send. `None` against an anchord
    /// too old to report it.
    pub fn size(&self) -> Option<(u16, u16)> {
        self.size
    }

    /// Reads until `pending` holds at least `want` bytes. `false` means the
    /// stream hit EOF first.
    ///
    /// Cancel-safe, which is the point: `read_buf` appends into `pending`,
    /// so every byte taken off the socket is retained even if this future is
    /// dropped mid-read and called again later.
    async fn fill_to(&mut self, want: usize) -> Result<bool> {
        while self.pending.len() < want {
            // `read_buf` writes into spare capacity; with none it would
            // return 0 and be mistaken for EOF.
            self.pending.reserve(want - self.pending.len());
            let n = self
                .reader
                .read_buf(&mut self.pending)
                .await
                .context("reading a frame")?;
            if n == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Reads the next frame. `None` once the stream ends — which, per
    /// anchor's protocol, is also how a subscriber that fell behind is told
    /// to reconnect and replay from its last offset.
    ///
    /// Cancel-safe: see `pending`.
    pub async fn next_frame(&mut self) -> Result<Option<AnchorFrame>> {
        if !self.fill_to(frame::HEADER_LEN).await? {
            // A clean end is EOF on a frame boundary. EOF with bytes still
            // buffered is a truncated frame, which is an error rather than
            // an orderly close.
            return if self.pending.is_empty() {
                Ok(None)
            } else {
                Err(anyhow::anyhow!("anchor's stream ended mid-frame"))
            };
        }
        let header = self
            .pending
            .get(..frame::HEADER_LEN)
            .context("a frame header is 13 bytes")?;
        let tag = *header.first().context("a frame header is 13 bytes")?;
        let offset = u64::from_be_bytes(
            header
                .get(1..9)
                .and_then(|s| <[u8; 8]>::try_from(s).ok())
                .context("a frame header is 13 bytes")?,
        );
        let len = u32::from_be_bytes(
            header
                .get(9..13)
                .and_then(|s| <[u8; 4]>::try_from(s).ok())
                .context("a frame header is 13 bytes")?,
        ) as usize;

        let total = frame::HEADER_LEN + len;
        if !self.fill_to(total).await? {
            return Err(anyhow::anyhow!("anchor's stream ended mid-frame"));
        }
        let payload: Vec<u8> = self
            .pending
            .get(frame::HEADER_LEN..total)
            .context("the frame was just filled to this length")?
            .to_vec();
        self.pending.drain(..total);

        Ok(Some(match tag {
            frame::OUTPUT => AnchorFrame::Output {
                offset,
                bytes: payload,
            },
            frame::EXIT => AnchorFrame::Exit {
                code: <[u8; 4]>::try_from(payload.as_slice())
                    .ok()
                    .map(i32::from_le_bytes),
            },
            frame::LEASE => AnchorFrame::Lease {
                held: payload.first() == Some(&1),
            },
            // Two u16s big-endian. A short payload is a daemon speaking a
            // dialect this build does not know, which is skipped rather
            // than guessed at — the same reasoning as an unknown tag.
            frame::RESIZE => match (
                payload.get(..2).and_then(|b| <[u8; 2]>::try_from(b).ok()),
                payload.get(2..4).and_then(|b| <[u8; 2]>::try_from(b).ok()),
            ) {
                (Some(cols), Some(rows)) => AnchorFrame::Resized {
                    cols: u16::from_be_bytes(cols),
                    rows: u16::from_be_bytes(rows),
                },
                _ => {
                    return Ok(Some(AnchorFrame::Output {
                        offset,
                        bytes: Vec::new(),
                    }))
                }
            },
            // An unknown tag is a newer daemon, not a broken one: skipping
            // it keeps an older teleport working against it.
            _ => {
                return Ok(Some(AnchorFrame::Output {
                    offset,
                    bytes: Vec::new(),
                }))
            }
        }))
    }

    /// Claims anchor's control lease for this connection, preempting
    /// whoever holds it.
    ///
    /// `attach` deliberately never preempts, so this is the only way to take
    /// control of a session someone else is driving. Anchor answers with a
    /// `lease` frame to this connection and to every other one attached.
    pub async fn claim_control(&mut self) -> Result<()> {
        let mut out = Vec::with_capacity(frame::HEADER_LEN + 1);
        out.push(frame::LEASE);
        out.extend_from_slice(&0_u64.to_be_bytes());
        out.extend_from_slice(&1_u32.to_be_bytes());
        out.push(1);
        self.writer
            .write_all(&out)
            .await
            .context("claiming control")?;
        self.writer.flush().await.context("flushing a claim")
    }

    /// Resizes the PTY from this connection.
    ///
    /// Sent here rather than as a control op because anchor's lease is
    /// per-connection: a control op arrives on a different connection and
    /// cannot prove it holds the lease, so anchor refuses it while someone
    /// does. Only the holder's resize is applied.
    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&cols.to_be_bytes());
        payload.extend_from_slice(&rows.to_be_bytes());

        let mut out = Vec::with_capacity(frame::HEADER_LEN + payload.len());
        out.push(frame::RESIZE);
        out.extend_from_slice(&0_u64.to_be_bytes());
        out.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(&payload);
        self.writer.write_all(&out).await.context("resizing")?;
        self.writer.flush().await.context("flushing a resize")
    }

    /// Sends input to the PTY. Only the lease holder's input is applied;
    /// anchor silently ignores the rest.
    ///
    /// Split across frames at `frame::MAX_INBOUND`. A browser paste is the
    /// one thing a client sends that is not keystroke-sized, and a single
    /// oversized frame would not merely be refused — anchor drops the
    /// connection on the header, taking the whole attach down and killing
    /// the terminal. The PTY sees one byte stream either way; frame
    /// boundaries are not message boundaries.
    pub async fn send_input(&mut self, bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks(frame::MAX_INBOUND) {
            let mut out = Vec::with_capacity(frame::HEADER_LEN + chunk.len());
            out.push(frame::INPUT);
            out.extend_from_slice(&0_u64.to_be_bytes());
            // Never truncates: a chunk is at most `MAX_INBOUND`.
            out.extend_from_slice(&u32::try_from(chunk.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(chunk);
            self.writer.write_all(&out).await.context("sending input")?;
        }
        // `chunks` yields nothing for an empty slice, so an empty write sends
        // no frame at all. That is right: a zero-length input frame would ask
        // anchor to deliver nothing, and the PTY cannot tell it apart from
        // not having been written to.
        self.writer.flush().await.context("flushing input")
    }
}

/// How long a failed write waits for the reply that may already be on the
/// wire.
///
/// Only the error path uses it. A write that failed is no promise a reply is
/// coming, so an unbounded read there could hang a request forever on a peer
/// that died without closing. Generous enough that a reply already written
/// is never missed.
const REPLY_AFTER_WRITE_ERROR: std::time::Duration = std::time::Duration::from_secs(5);

/// Sends one control line and reads its reply, preferring a reply that
/// arrived to the write error that hid it.
///
/// Anchord answers a control line past its 1 MiB cap with `too_large` and
/// *then* closes, so the client's own write usually fails with megabytes
/// still in flight while the reply is already on the wire. Returning that
/// write error unread turns the one failure anchor took care to name into a
/// broken pipe, indistinguishable from the daemon dying. Anchor's
/// `docs/04-protocol.md` is explicit: read it before acting on the write
/// error.
///
/// No reply means the write error was the truth, and it is what comes back.
async fn exchange(
    out: &mut WriteHalf,
    reader: &mut BufReader<ReadHalf>,
    body: &Value,
) -> Result<Value> {
    let Err(write_error) = write_request(out, body).await else {
        return read_response(reader).await;
    };
    match tokio::time::timeout(REPLY_AFTER_WRITE_ERROR, read_response(reader)).await {
        Ok(Ok(response)) => Ok(response),
        // Nothing readable, or nothing in time: the write error is the only
        // account of what happened, so it is the one reported.
        Ok(Err(_)) | Err(_) => Err(write_error),
    }
}

async fn write_request(out: &mut WriteHalf, body: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(body).context("encoding a request")?;
    line.push(b'\n');
    out.write_all(&line).await.context("sending a request")?;
    out.flush().await.context("flushing a request")
}

async fn read_response(reader: &mut BufReader<ReadHalf>) -> Result<Value> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .await
        .context("reading a response")?
        == 0
    {
        bail!("anchord closed the connection without responding");
    }
    serde_json::from_str(&line).context("decoding a response")
}

/// Turns anchor's `ok:false` envelope into an error carrying its code, so a
/// caller can match on `not_found` rather than on a message.
fn check_ok(response: &Value) -> Result<()> {
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    let code = response
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("error");
    let message = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("anchord refused the request");
    bail!("{code}: {message}")
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use tokio::io::{AsyncRead, AsyncWrite};

    use super::{AnchorClient, AnchorSpec, BTreeMap, PathBuf};

    /// Anchord's cap on one control line, from its `docs/04-protocol.md`.
    const MAX_CONTROL_LINE: usize = 1024 * 1024;

    /// A stand-in for anchord doing the one thing this test is about: it
    /// reads up to the cap, answers `too_large`, and closes **without
    /// draining the rest of the line**. That last part is what makes the
    /// client's own write fail with the reply already on the wire, which is
    /// the situation anchor's protocol doc warns clients about.
    ///
    /// Generic over the stream so both transports run the same server.
    async fn serve_too_large_then_close<S: AsyncRead + AsyncWrite + Unpin>(stream: S) {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = super::BufReader::new(read_half);
        let mut seen = 0_usize;
        let mut buf = vec![0_u8; 64 * 1024];
        while seen < MAX_CONTROL_LINE {
            match super::AsyncReadExt::read(&mut reader, &mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => seen += n,
            }
        }
        let reply = br#"{"id":1,"ok":false,"error":{"code":"too_large","message":"control line is longer than 1 MiB"}}
"#;
        if super::AsyncWriteExt::write_all(&mut write_half, reply)
            .await
            .is_ok()
        {
            drop(super::AsyncWriteExt::flush(&mut write_half).await);
        }
        // Both halves drop here: the connection closes with megabytes of
        // the client's request still unread.
    }

    /// A fake anchord on a fresh `AF_UNIX` socket, and the socket's path.
    #[cfg(unix)]
    fn fake_anchord(tag: &str) -> (PathBuf, tokio::task::JoinHandle<()>) {
        // Short path on purpose: a unix socket address holds ~104 bytes.
        let dir = PathBuf::from(format!("/tmp/tp-anchor-{tag}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("creating the socket dir");
        let socket = dir.join("anchor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("binding the fake anchord");
        let server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                serve_too_large_then_close(stream).await;
            }
        });
        (socket, server)
    }

    /// A fake anchord on a fresh named pipe, and the pipe's name.
    ///
    /// `first_pipe_instance`, as anchor's own server must (its
    /// `docs/09-windows.md` §4.2): a name already taken is a failure here,
    /// not a pipe to share with whoever took it.
    #[cfg(windows)]
    fn fake_anchord(tag: &str) -> (PathBuf, tokio::task::JoinHandle<()>) {
        use tokio::net::windows::named_pipe::ServerOptions;

        let name = format!(r"\\.\pipe\tp-anchor-{tag}-{}", std::process::id());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .expect("creating the fake anchord's pipe");
        let task = tokio::spawn(async move {
            if server.connect().await.is_ok() {
                serve_too_large_then_close(server).await;
            }
        });
        (PathBuf::from(name), task)
    }

    /// A control line past anchor's cap surfaces as the `too_large` anchord
    /// answered with -- a 400 -- rather than as the broken pipe that answer
    /// races.
    ///
    /// Runs on both transports, and on Windows it is the first test to put
    /// bytes through the named-pipe arm: the race it pins is the one most
    /// likely to differ between them, because it depends on what a write
    /// into a peer that stopped reading does.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_oversized_control_line_reports_the_reply_not_the_write_error() {
        let (endpoint, server) = fake_anchord("toolarge");

        // `create` carries the spec, and the spec carries the environment:
        // the one control line teleport can realistically make too big.
        let mut env = BTreeMap::new();
        env.insert("TELEPORT_BIG".to_owned(), "x".repeat(4 * MAX_CONTROL_LINE));
        let spec = AnchorSpec {
            command: "/bin/sh".to_owned(),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            env,
            cols: 80,
            rows: 24,
            login_shell: false,
        };

        let client = AnchorClient::new(&endpoint);
        let error = client
            .create(&spec, &BTreeMap::new())
            .await
            .expect_err("an oversized control line must not succeed");

        server.abort();
        #[cfg(unix)]
        if let Some(dir) = endpoint.parent() {
            std::fs::remove_dir_all(dir).expect("cleaning up the socket dir");
        }

        let text = error.to_string();
        assert!(
            text.starts_with("too_large:"),
            "the write error hid anchord's reply: {text}"
        );
        assert_eq!(
            crate::anchor_bridge::api_error(&error)
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST,
            "`too_large` must reach the client as a 400, not a 500"
        );
    }

    /// Nothing listening is an error naming the endpoint, on either
    /// transport -- never a hang, and never an empty answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_anchord_is_a_connect_error_naming_the_endpoint() {
        #[cfg(unix)]
        let endpoint = PathBuf::from(format!("/tmp/tp-anchor-none-{}.sock", std::process::id()));
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(r"\\.\pipe\tp-anchor-none-{}", std::process::id()));

        let error = AnchorClient::new(&endpoint)
            .list(&BTreeMap::new())
            .await
            .expect_err("nothing is listening");
        let text = format!("{error:#}");
        assert!(
            text.contains(&endpoint.display().to_string()),
            "the error should say where it looked: {text}"
        );
    }
}
