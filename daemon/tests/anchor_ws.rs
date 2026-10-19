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

/// The only test in this file that is not itself gated on `ANCHORD_BIN`.
/// See the twin in `anchor_http.rs` for why a skip here reports as a pass.
#[test]
fn ci_must_supply_an_anchord_binary() {
    if std::env::var_os("CI").is_some() {
        assert!(
            anchord_bin().is_some(),
            "ANCHORD_BIN is unset under CI: every anchor test would report ok without running"
        );
    }
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
    let client = teleportd::anchor_client::AnchorClient::new(daemon.state.anchor.socket());
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

/// One session as teleport's own HTTP view reports it.
async fn get_session(daemon: &support::Daemon, id: &str) -> Value {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/sessions/{id}"))
        .header(axum::http::header::HOST, "127.0.0.1")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", support::TOKEN),
        )
        .body(axum::body::Body::empty())
        .expect("valid request");
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = tower::ServiceExt::oneshot(router, req)
        .await
        .expect("router call");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("a session view")
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

/// The session list reports the same controller the WebSocket does.
///
/// `ready` has always carried the controller from teleport's lease registry
/// (`ws_anchor.rs`), but the list and get views hardcoded `controller: null`
/// and `subscribers: 0` on the anchor path. One attached browser could
/// therefore be told it holds control while the list said nobody did — two
/// answers to one question, and the UI reads both (#gap 3).
#[tokio::test(flavor = "multi_thread")]
async fn the_session_list_agrees_with_the_stream_about_who_is_in_control() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("controller");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(&daemon, "stty raw -echo; echo ctl-ready; cat").await;

    // Before anyone attaches: nobody is in control, nobody is watching.
    let view = get_session(&daemon, &id).await;
    assert_eq!(view["controller"], Value::Null, "nobody has attached yet");
    assert_eq!(view["subscribers"], 0);

    let mut ws = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-a&client_name=Ada"
    )))
    .await;
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["control"], true);
    assert_eq!(
        ready["controller"], "Ada",
        "the stream names the controller"
    );

    // The same question, asked of the list, must get the same answer.
    let view = get_session(&daemon, &id).await;
    assert_eq!(
        view["controller"], "Ada",
        "the list disagrees with the stream about who is in control"
    );
    assert_eq!(
        view["subscribers"], 1,
        "one browser is attached and the list says {}",
        view["subscribers"]
    );
}

/// A resize by someone who is not this browser still reaches it.
///
/// Geometry is not in the output bytes, so without a frame saying so a
/// client whose session is resized by another anchord client — an `anchor`
/// CLI, or a second teleport — keeps rendering at a width the PTY no longer
/// has. Teleport could not observe it at all until anchor made `0x05`
/// travel both ways (anchor 77dde06); the spec carried it as a known
/// limitation until then.
///
/// Also pins the two halves that are easy to get subtly wrong: the attach
/// reply carries the starting size, so a session nobody ever resizes still
/// has one, and the frame carries the size anchor *applied* rather than the
/// size anyone asked for.
#[tokio::test(flavor = "multi_thread")]
async fn a_resize_by_another_client_reaches_an_attached_browser() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("thirdpartyresize");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(&daemon, "stty raw -echo; echo size-ready; cat").await;

    let mut ws = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=observe&client_id=browser-b&client_name=B"
    )))
    .await;
    let ready = next_json(&mut ws).await;
    // From the attach reply: nobody has resized anything yet, and this
    // browser still knows how wide the session is.
    assert_eq!(ready["cols"], 80, "the attach reply carried no size");
    assert_eq!(ready["rows"], 24);
    read_until(&mut ws, "size-ready").await;

    // A different anchord client entirely — not this browser, and not
    // teleport's WebSocket path.
    let client = teleportd::anchor_client::AnchorClient::new(anchord.socket());
    client.resize(&id, 120, 40).await.expect("resize");

    // The observer is told, despite having done nothing and holding no
    // lease. Skipping output frames to find the control frame.
    let resized = loop {
        let frame = next_json(&mut ws).await;
        if frame["type"] == "resized" {
            break frame;
        }
    };
    assert_eq!(resized["cols"], 120, "wrong width: {resized}");
    assert_eq!(resized["rows"], 40, "wrong height: {resized}");
}

/// The next close frame's code, skipping anything still queued ahead of it.
#[expect(
    clippy::panic,
    reason = "test helper asserting an unexpected value; panic! is the idiomatic way to fail with it attached"
)]
async fn next_close(ws: &mut WsStream) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the socket never closed");
        match ws.next().await {
            Some(Ok(Message::Close(frame))) => return frame.map_or(0, |frame| frame.code.into()),
            Some(Ok(_)) => {}
            // A close frame the peer sent and then dropped the TCP
            // connection behind reaches tungstenite as this, not as a
            // frame; the assertion below is about the frame, so treat
            // losing it as the failure it is.
            Some(Err(e)) => panic!("ws error before the close frame: {e}"),
            None => panic!("the socket ended without a close frame"),
        }
    }
}

/// A cursor past the head is the one attach failure teleport names itself,
/// and the client acts on the name: `offset_ahead` is what makes it drop a
/// stale cursor and restart at the default tail. Asserted by value, close
/// code included, because that is the whole content of the contract
/// (docs/04-api-protocol.md#error-codes).
#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_past_the_head_is_offset_ahead_and_nothing_else() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("offsetahead");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(&daemon, "stty raw -echo; echo ahead-ready; cat").await;

    // Far past anything this session could have produced, so the verdict
    // does not depend on how much of the banner has landed yet.
    let mut ws = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=observe&after=999999999&client_id=browser-a"
    )))
    .await;

    let error = next_json(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "offset_ahead", "wrong code: {error}");
    assert!(
        error["next_offset"].is_u64(),
        "the client is told where the daemon actually is: {error}"
    );
    assert_eq!(next_close(&mut ws).await, 1008, "wrong close code");
}

/// A controller's lease survives its socket dying, on the anchor path too.
///
/// `ws_protocol.rs` proves this for the in-process path three ways
/// (`disconnect_grace_resumes_for_the_same_client_but_never_wins_a_race`,
/// `grace_expiry_frees_the_lease_with_no_auto_grant`,
/// `grace_expiry_tells_the_client_it_was_taken_from`). Nothing proved it for
/// the anchor path, so nothing proved `ws_anchor.rs:418` actually reaches
/// `begin_control_grace` when a socket drops — which is the half that leaked
/// in #101 and the reason `is_tracking` exists.
///
/// The load-bearing assertion is the *second* client, not the reconnect. A
/// lease that was simply freed on disconnect would also let the original
/// client take control again on reconnect, so resuming proves nothing on its
/// own. Only a bystander being refused in the meantime distinguishes "held
/// in grace for A" from "free, and A happened to ask first".
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_controller_keeps_its_lease_through_the_grace_window() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("grace");
    anchord.ready().await;
    let config = Config {
        // Long enough that the assertions below are not racing the clock on
        // a loaded machine, short enough that the test is not itself a sleep.
        control_grace_ms: 4_000,
        ..Default::default()
    };
    let daemon = support::spawn_on_anchor(config, None, anchord.socket()).await;
    let id = create(&daemon, "stty raw -echo; echo grace-ready; cat").await;

    let mut first = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-a&client_name=A"
    )))
    .await;
    assert_eq!(next_json(&mut first).await["control"], true);
    read_until(&mut first, "grace-ready").await;

    // An ordinary disconnect -- the socket dies, the human did not release.
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The discriminating assertion: a different client attaching for control
    // inside A's grace window must be refused. Attach never preempts, and a
    // lease held in grace is not free.
    let mut bystander = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-b&client_name=B"
    )))
    .await;
    let ready = next_json(&mut bystander).await;
    assert_eq!(
        ready["control"], false,
        "a lease inside its grace window is not free: {ready}"
    );
    assert_eq!(
        ready["controller"], "A",
        "the grace window still names the client it is being held for: {ready}"
    );
    drop(bystander);

    // And the original client resumes it by reconnecting under the same id.
    let mut again = connect(&daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?mode=control&client_id=browser-a&client_name=A"
    )))
    .await;
    let resumed = next_json(&mut again).await;
    assert_eq!(
        resumed["control"], true,
        "the same client_id must resume its lease inside the grace window: {resumed}"
    );

    // Resuming is not just a flag: input has to reach the PTY again.
    again
        .send(Message::Binary(b"echo resumed-types\n".to_vec().into()))
        .await
        .expect("send input");
    read_until(&mut again, "resumed-types").await;
}

/// The HTTP status a `/stream` upgrade is refused with, or `None` if it
/// succeeded.
///
/// `connect` asserts the handshake works; this one is about the handshake
/// failing, and specifically about *which* refusal the client is given.
#[expect(
    clippy::panic,
    reason = "test helper asserting an unexpected value; panic! is the idiomatic way to fail with it attached"
)]
async fn upgrade_status(url: &str) -> Option<u16> {
    let mut request = url.into_client_request().expect("valid ws url");
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", support::TOKEN)
            .parse()
            .expect("valid header"),
    );
    match tokio_tungstenite::connect_async(request).await {
        Ok(_) => None,
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Some(response.status().as_u16())
        }
        Err(e) => panic!("the upgrade failed for an unexpected reason: {e}"),
    }
}

/// An anchord that has gone away is an outage, not a missing session.
///
/// `api.rs` already draws this distinction for the session list and says why
/// in a comment: an empty list and an unreachable backend are "the difference
/// between a visible outage and silently vanished work". `/stream` collapses
/// both into `404`, and a browser told `404` has been told the session is
/// gone -- so it stops reconnecting, during the one event the anchor path
/// exists to survive.
#[tokio::test(flavor = "multi_thread")]
async fn an_anchord_outage_is_not_a_missing_session() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let mut anchord = Anchord::start("outage");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(&daemon, "echo up; cat").await;

    // Baseline: with anchord up, the upgrade succeeds. Without this the test
    // could pass on a session id that was never valid.
    let url = daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?mode=observe"));
    assert_eq!(upgrade_status(&url).await, None, "baseline: should attach");

    // anchord dies underneath a session that is still, from the user's point
    // of view, running -- a restart, an upgrade, a crash.
    anchord.child.kill().expect("killing anchord");
    anchord.child.wait().expect("reaping anchord");

    // What the REST surface says.
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/sessions")
        .header(axum::http::header::HOST, "127.0.0.1")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", support::TOKEN),
        )
        .body(axum::body::Body::empty())
        .expect("valid request");
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let list_status = tower::ServiceExt::oneshot(router, req)
        .await
        .expect("router call")
        .status()
        .as_u16();

    // And what the WebSocket surface says about the same daemon, at the same
    // moment, about the same sessions.
    let stream_status = upgrade_status(&url).await;

    assert_eq!(
        list_status, 503,
        "the list already reports an unreachable anchord as an outage"
    );
    assert_eq!(
        stream_status,
        Some(503),
        "/stream must not call an outage a missing session: the list says \
         {list_status} for the same condition, and a client that believes a \
         404 stops reconnecting to a session that is still alive"
    );
}
