//! Append-only output log -- `<data_dir>/sessions/<id>/output.vt`.
//!
//! See docs/05-persistence.md#output-log for the durability rules and
//! docs/11-mvp-plan.md#m3--append-only-replay for the milestone this
//! implements. Raw PTY bytes, appended, never rewritten: no framing, no
//! headers, no escaping. The file's length *is* the replay index.
//!
//! The invariant every other module may rely on:
//!
//! ```text
//! file_length == min(next_offset, log_capped_at)   # cap = infinity when null
//! ```
//!
//! This module owns `next_offset`. That is deliberate: docs/03-pty-layer.md's
//! reader loop requires **persist, then advance the offset, then fan out**, so
//! the counter has to live behind the same call that does the write, or the
//! ordering is an unenforced comment. `session.rs` holds an `OutputLog` inside
//! its fan-out mutex and reads the start offset back out of [`append`].
//!
//! **Not in scope here:** bounded attach (`tail` / `max_replay_bytes`) is M4 --
//! this module serves whatever byte range it is asked for, clamped only to
//! what actually exists. Keeping the bound one layer up is what lets a VT
//! state snapshot replace a byte range later without a protocol change
//! (docs/04-api-protocol.md#the-vt-state-caveat--read-this-before-implementing).
//! Recording [`LogEvent`]s in `session_events` is M7; [`append`] returns them
//! rather than writing them anywhere.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub const LOG_FILE_NAME: &str = "output.vt";

/// Defaults from docs/05-persistence.md#size-cap. `config.toml` does not
/// exist yet; when it does, it overrides these rather than replacing them.
pub const DEFAULT_LOG_WARN_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_LOG_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// Never `fsync` per chunk -- that couples PTY drain rate to disk latency
/// (docs/05-persistence.md#output-log). Sync on this interval instead.
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub struct LogLimits {
    pub warn_bytes: u64,
    pub max_bytes: u64,
    pub sync_interval: Duration,
}

impl Default for LogLimits {
    fn default() -> Self {
        Self {
            warn_bytes: DEFAULT_LOG_WARN_BYTES,
            max_bytes: DEFAULT_LOG_MAX_BYTES,
            sync_interval: DEFAULT_SYNC_INTERVAL,
        }
    }
}

/// What SQLite remembered about this log before the daemon restarted. The
/// seam for docs/05-persistence.md#restart-recovery; `persistence.rs` (M7)
/// fills it in, and until then every open is a fresh one.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoredState {
    pub output_bytes: u64,
    pub log_capped_at: Option<u64>,
}

/// Something worth a `session_events` row (M7) or an operator-visible log
/// line. Returned rather than written, because [`append`] runs on the PTY
/// reader thread and nothing on that thread may touch SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    /// Crossed `log_warn_bytes`. Emitted once per log.
    Warned { output_bytes: u64 },
    /// Hit `log_max_bytes`. The file stops growing here; `next_offset` keeps
    /// advancing and live streaming continues.
    Capped { at: u64 },
    /// A write or sync failed. Persistence stops; the session keeps running
    /// (docs/05-persistence.md: `lost_reason='io_error'` is the one reason
    /// that can be set while the child is still alive).
    IoError { at: u64, error: String },
}

/// The result of one append. `start` is the offset of the chunk's first byte
/// -- the value the subscriber fan-out tags the chunk with.
#[derive(Debug)]
pub struct Appended {
    pub start: u64,
    pub events: Vec<LogEvent>,
}

/// The append side of one session's log. Single-writer by construction:
/// `session.rs` keeps it inside the mutex the reader loop takes.
pub struct OutputLog {
    path: PathBuf,
    file: File,
    limits: LogLimits,
    /// Authoritative output offset. Never rewinds -- not on a cap, not across
    /// a restart (docs/05-persistence.md#restart-recovery).
    next_offset: u64,
    /// Bytes actually on disk. Equal to `min(next_offset, log_capped_at)`
    /// except after an I/O error, where it is the honest, smaller truth.
    file_len: u64,
    log_capped_at: Option<u64>,
    /// Sticky: once a write fails we stop persisting rather than interleave
    /// a hole into a byte stream whose offsets are an index.
    io_failed: bool,
    warned: bool,
    last_sync: Instant,
}

impl OutputLog {
    /// Opens (creating if needed) `<dir>/output.vt` in append mode.
    ///
    /// `stored` is the SQLite row for this session, if there is one. Recovery
    /// takes `max(len(output.vt), stored output_bytes)`: the file cannot lie
    /// about bytes it holds, but for a *capped* log the file stopped growing
    /// while the column kept counting, and taking the file length there would
    /// rewind `next_offset` below offsets live clients already hold.
    pub fn open(dir: &Path, limits: LogLimits, stored: Option<StoredState>) -> Result<Self> {
        create_dir_owner_only(dir)?;
        let path = dir.join(LOG_FILE_NAME);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let file_len = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .len();

        let stored = stored.unwrap_or_default();
        let next_offset = file_len.max(stored.output_bytes);

        // A recorded cap wins. Otherwise: a full file is capped even if no row
        // said so, and `next_offset` running ahead of the file means
        // persistence stopped at `file_len` whatever the reason -- say so,
        // rather than leaving a null cap that claims those bytes are readable.
        let log_capped_at = match stored.log_capped_at {
            Some(at) => Some(at),
            None if file_len >= limits.max_bytes || next_offset > file_len => Some(file_len),
            None => None,
        };

        Ok(Self {
            path,
            file,
            limits,
            next_offset,
            file_len,
            log_capped_at,
            io_failed: false,
            warned: file_len >= limits.warn_bytes,
            last_sync: Instant::now(),
        })
    }

    /// Persists `bytes`, then advances the offset. Runs on the PTY reader
    /// thread; never blocks on anything but the write itself, and never
    /// fails upward -- a log that cannot be written must not kill a running
    /// child (docs/05-persistence.md).
    pub fn append(&mut self, bytes: &[u8]) -> Appended {
        let start = self.next_offset;
        let mut events = Vec::new();

        if !self.io_failed && self.log_capped_at.is_none() {
            // Fill the budget exactly, then stop: `log_capped_at` is always
            // `max_bytes` for a log that got there by growing.
            let room = self.limits.max_bytes.saturating_sub(self.file_len);
            let fits = room.min(bytes.len() as u64) as usize;

            if fits > 0 {
                match self.file.write_all(&bytes[..fits]) {
                    Ok(()) => self.file_len += fits as u64,
                    Err(e) => self.fail(&mut events, &e),
                }
            }

            if !self.io_failed {
                if !self.warned && self.file_len >= self.limits.warn_bytes {
                    self.warned = true;
                    events.push(LogEvent::Warned { output_bytes: self.file_len });
                }
                if fits < bytes.len() {
                    self.log_capped_at = Some(self.file_len);
                    events.push(LogEvent::Capped { at: self.file_len });
                }
            }
        }

        // Persist first, advance second -- a subscriber must never see an
        // offset for bytes that are not yet readable
        // (docs/03-pty-layer.md#reader-loop).
        self.next_offset = start + bytes.len() as u64;
        self.maybe_sync(&mut events);

        Appended { start, events }
    }

    /// The authoritative output offset. Bytes below it have been handed out;
    /// bytes at or above it do not exist yet.
    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    /// Offset at which persistence stopped, or `None` while the whole stream
    /// is still on disk.
    pub fn log_capped_at(&self) -> Option<u64> {
        self.log_capped_at
    }

    /// One past the last byte a reader can actually get. Every range read
    /// clamps to this; it is the file length, so it is true under a cap and
    /// under an I/O error alike.
    pub fn readable_end(&self) -> u64 {
        self.file_len
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A fresh read handle. Independent of the append handle -- replay seeks
    /// and reads without touching the writer's file position.
    pub fn reader(&self) -> io::Result<LogReader> {
        LogReader::open(&self.path)
    }

    /// Flushes to disk. Call on session close; the periodic case is handled
    /// inside [`append`].
    pub fn sync(&mut self) -> Vec<LogEvent> {
        let mut events = Vec::new();
        if let Err(e) = self.file.sync_data() {
            self.fail(&mut events, &e);
        }
        self.last_sync = Instant::now();
        events
    }

    fn maybe_sync(&mut self, events: &mut Vec<LogEvent>) {
        if self.io_failed || self.last_sync.elapsed() < self.limits.sync_interval {
            return;
        }
        if let Err(e) = self.file.sync_data() {
            self.fail(events, &e);
        }
        self.last_sync = Instant::now();
    }

    fn fail(&mut self, events: &mut Vec<LogEvent>, error: &io::Error) {
        if self.io_failed {
            return;
        }
        self.io_failed = true;
        events.push(LogEvent::IoError { at: self.file_len, error: error.to_string() });
    }
}

/// The read side. Cheap to create, one per replay.
pub struct LogReader {
    file: File,
}

impl LogReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self { file: File::open(path)? })
    }

    /// Reads `[from, to)`. Returns fewer bytes than asked for only if the
    /// file is shorter than `to` -- callers clamp `to` to
    /// [`OutputLog::readable_end`] under the fan-out lock, so a short read
    /// here means someone read past what they accounted for.
    pub fn read_range(&mut self, from: u64, to: u64) -> io::Result<Vec<u8>> {
        if to <= from {
            return Ok(Vec::new());
        }
        let len = to - from;
        self.file.seek(SeekFrom::Start(from))?;
        let mut buf = Vec::with_capacity(len as usize);
        Read::by_ref(&mut self.file).take(len).read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// `<data_dir>` is already `0700`; session directories under it match rather
/// than relying on the parent (docs/06-security.md).
fn create_dir_owner_only(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting owner-only permissions on {}", dir.display()))?;
    }
    Ok(())
}
