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
use tokio::net::UnixStream;

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
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connecting to {}", self.socket.display()))?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let request = json!({
            "id": 1,
            "op": "attach",
            "session": id,
            "from": from,
            "control": control,
        });
        write_request(&mut write_half, &request).await?;
        let response = read_response(&mut reader).await?;
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

    /// One control request on its own connection.
    async fn call(&self, op: &str, body: Value) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connecting to {}", self.socket.display()))?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // Built rather than indexed into: `Value`'s index panics on a
        // non-object, and every caller here passes an object literal.
        let mut request = serde_json::Map::new();
        request.insert("id".to_owned(), json!(1));
        request.insert("op".to_owned(), json!(op));
        if let Value::Object(fields) = body {
            request.extend(fields);
        }
        write_request(&mut write_half, &Value::Object(request)).await?;
        let response = read_response(&mut reader).await?;
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
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
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
    pub async fn send_input(&mut self, bytes: &[u8]) -> Result<()> {
        let mut out = Vec::with_capacity(frame::HEADER_LEN + bytes.len());
        out.push(frame::INPUT);
        out.extend_from_slice(&0_u64.to_be_bytes());
        out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(bytes);
        self.writer.write_all(&out).await.context("sending input")?;
        self.writer.flush().await.context("flushing input")
    }
}

async fn write_request(out: &mut tokio::net::unix::OwnedWriteHalf, body: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(body).context("encoding a request")?;
    line.push(b'\n');
    out.write_all(&line).await.context("sending a request")?;
    out.flush().await.context("flushing a request")
}

async fn read_response(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Result<Value> {
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
