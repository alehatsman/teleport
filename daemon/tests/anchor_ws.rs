//! Teleport's WebSocket protocol, served by an external `anchord`.
//!
//! The last piece of anchor's M4: a browser speaks teleport's protocol
//! unchanged while the PTY lives in another process. Gated on `ANCHORD_BIN`
//! like the other anchor fixtures.

#![cfg(unix)]

mod support;

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use teleportd::config::Config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn anchord_bin() -> Option<PathBuf> {
    let raw = std::env::var_os("ANCHORD_BIN")?;
    let path = PathBuf::from(&raw);
    assert!(
        path.is_file(),
        "ANCHORD_BIN is set to {raw:?}, which is not a file"
    );
    Some(path)
}

struct Anchord {
    child: Child,
    dir: PathBuf,
}

impl Anchord {
    fn start(tag: &str) -> Self {
        let dir = PathBuf::from(format!("/tmp/tp-ws-{}-{tag}", std::process::id()));
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort cleanup of a previous run"
        )]
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).expect("creating the data dir");
        let child = Command::new(anchord_bin().expect("checked by the caller"))
            .env("XDG_DATA_HOME", &dir)
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("ANCHOR_LOG", "warn")
            .spawn()
            .expect("spawning anchord");
        Self { child, dir }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("run/anchor.sock")
    }

    async fn ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while tokio::net::UnixStream::connect(self.socket())
            .await
            .is_err()
        {
            assert!(Instant::now() < deadline, "anchord never came up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Anchord {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort teardown in Drop; a failure cannot be propagated"
    )]
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn connect(url: &str) -> WsStream {
    let mut request = url.into_client_request().expect("valid ws url");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", support::TOKEN)
            .parse()
            .expect("valid header value"),
    );
    let (stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect");
    stream
}

/// The next control frame, skipping binary output in between.
#[expect(
    clippy::panic,
    reason = "test helper asserting an unexpected value; panic! is the idiomatic way to fail with it attached"
)]
async fn next_json(ws: &mut WsStream) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no control frame arrived");
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).expect("a JSON control frame")
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("ws error: {e}"),
            None => panic!("the socket closed"),
        }
    }
}

/// Accumulates binary output until `needle` appears.
#[expect(
    clippy::panic,
    reason = "test helper asserting an unexpected value; panic! is the idiomatic way to fail with it attached"
)]
async fn read_until(ws: &mut WsStream, needle: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut acc = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?}; got {:?}",
            String::from_utf8_lossy(&acc)
        );
        match ws.next().await {
            // Every binary frame is an 8-byte big-endian offset then bytes.
            Some(Ok(Message::Binary(frame))) => {
                let Some(payload) = frame.get(8..) else {
                    continue;
                };
                acc.extend_from_slice(payload);
                if acc.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    return acc;
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("ws error: {e}"),
            None => panic!("the socket closed"),
        }
    }
}

async fn create(daemon: &support::Daemon, script: &str) -> String {
    let client = teleportd::anchor_client::AnchorClient::new(
        daemon.state.anchor.as_ref().expect("anchor").socket(),
    );
    client
        .create(
            &teleportd::anchor_client::AnchorSpec {
                command: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), script.to_owned()],
                cwd: std::env::temp_dir(),
                env: std::collections::BTreeMap::new(),
                cols: 80,
                rows: 24,
                login_shell: false,
            },
            &std::collections::BTreeMap::new(),
        )
        .await
        .expect("create")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_attaches_through_teleport_to_a_session_in_anchord() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("attach");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(&daemon, "stty raw -echo; echo ws-ready; cat").await;

    let mut ws = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-a&client_name=A"
    )))
    .await;

    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["session_id"], id.as_str());
    assert_eq!(ready["control"], true, "the lease was free");
    assert_eq!(ready["cols"], 80);

    // Replay carries what the session produced before this socket existed.
    read_until(&mut ws, "ws-ready").await;

    // Typing reaches the PTY in the other process and comes back.
    ws.send(Message::Binary(b"echo through-the-proxy\n".to_vec().into()))
        .await
        .expect("send input");
    read_until(&mut ws, "through-the-proxy").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn control_moves_between_browsers_and_both_are_told() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("lease");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(&daemon, "stty raw -echo; echo lease-ready; cat").await;

    let mut first = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-a&client_name=A"
    )))
    .await;
    assert_eq!(next_json(&mut first).await["control"], true);

    // A second browser attaches asking for control: teleport's lease never
    // preempts on attach, so it observes.
    let mut second = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-b&client_name=B"
    )))
    .await;
    let ready = next_json(&mut second).await;
    assert_eq!(ready["control"], false, "attach must not preempt");
    assert_eq!(ready["controller"], "A");

    read_until(&mut first, "lease-ready").await;
    read_until(&mut second, "lease-ready").await;

    // It claims control explicitly.
    second
        .send(Message::Text(
            json!({ "type": "claim_control" }).to_string().into(),
        ))
        .await
        .expect("claim");

    // The claimer is granted it; the first browser is told it lost it.
    let granted = next_json(&mut second).await;
    assert_eq!(granted["type"], "control_granted");

    // The same frame shape ws.rs sends, asserted against teleport's own
    // field names rather than ones invented for the anchor path — a client
    // must not be able to tell the two backends apart.
    let revoked = next_json(&mut first).await;
    assert_eq!(revoked["type"], "control_revoked");
    assert_eq!(revoked["client_id"], "browser-b");
    assert_eq!(revoked["to"], "B");

    // Input now follows the lease across the process boundary.
    second
        .send(Message::Binary(b"echo second-types\n".to_vec().into()))
        .await
        .expect("send input");
    read_until(&mut second, "second-types").await;

    // And the preempted browser can no longer type.
    first
        .send(Message::Binary(b"echo must-not-appear\n".to_vec().into()))
        .await
        .expect("send input");
    second
        .send(Message::Binary(b"echo marker-after\n".to_vec().into()))
        .await
        .expect("send input");
    let seen = read_until(&mut second, "marker-after").await;
    assert!(
        !seen.windows(15).any(|w| w == b"must-not-appear"),
        "a preempted browser must not reach the PTY"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_observer_is_refused_resize_but_still_sees_output() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("observe");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(&daemon, "stty raw -echo; echo observed; cat").await;

    let mut controller = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-a&client_name=A"
    )))
    .await;
    assert_eq!(next_json(&mut controller).await["control"], true);

    let mut observer = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=observe&client_id=browser-b&client_name=B"
    )))
    .await;
    assert_eq!(next_json(&mut observer).await["control"], false);

    // It sees everything the controller does.
    read_until(&mut observer, "observed").await;

    // But cannot resize.
    observer
        .send(Message::Text(
            json!({ "type": "resize", "cols": 120, "rows": 40 })
                .to_string()
                .into(),
        ))
        .await
        .expect("resize");
    let error = next_json(&mut observer).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "not_controller");
}

/// A paste larger than one anchor frame survives, and the attach survives
/// with it.
///
/// Anchor caps an inbound frame at 64 KiB and drops the *connection* on the
/// header alone when one claims more — not the frame, the connection, since
/// an attached connection ends as soon as either direction does. Teleport
/// used to forward a browser's binary message as a single frame, so a large
/// paste killed the terminal it was pasted into (#97).
///
/// `cat` echoes raw, so the bytes come back as proof the whole paste reached
/// the PTY in order rather than being truncated at a frame boundary.
#[tokio::test(flavor = "multi_thread")]
async fn a_paste_larger_than_one_frame_reaches_the_pty_whole() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("bigpaste");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(&daemon, "stty raw -echo; echo paste-ready; cat").await;

    let mut ws = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-a&client_name=A"
    )))
    .await;
    assert_eq!(next_json(&mut ws).await["control"], true);
    read_until(&mut ws, "paste-ready").await;

    // Three frames' worth, so the split is exercised rather than straddled:
    // one full chunk, a second full chunk, and a short remainder.
    let filler = 150_000;
    let mut paste = vec![b'z'; filler];
    paste.extend_from_slice(b"end-of-paste\n");
    ws.send(Message::Binary(paste.into()))
        .await
        .expect("send a large paste");

    // Arriving at all means the connection outlived the paste. The filler
    // immediately before the marker means the bytes arrived whole and in
    // order, rather than a chunk being dropped or reordered at a boundary.
    let seen = read_until(&mut ws, "end-of-paste").await;
    let marker = seen
        .windows(12)
        .position(|w| w == b"end-of-paste")
        .expect("read_until returned once it matched");
    let run = seen
        .get(marker - filler..marker)
        .expect("the filler precedes the marker");
    assert!(
        run.iter().all(|b| *b == b'z'),
        "the {filler} bytes before the marker were not the filler that was pasted"
    );
}
