//! M4's WS protocol gate -- the "Protocol tests" subset of
//! docs/10-testing.md#3-protocol-tests that needs a real socket (framing,
//! control lease, attach race, Origin/Host, keepalive semantics). HTTP-only
//! checks live in `http_api.rs`.

mod support;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const TIMEOUT: Duration = Duration::from_secs(5);

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connects with `Authorization: Bearer <token>` and, unless `None`, an
/// `Origin` header -- the two knobs every Origin/credential test in this
/// file needs to vary.
async fn connect(
    url: &str,
    token: Option<&str>,
    origin: Option<&str>,
) -> Result<WsStream, tokio_tungstenite::tungstenite::Error> {
    let mut request = url.into_client_request().expect("valid ws url");
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    }
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", origin.parse().unwrap());
    }
    let (stream, _response) = tokio_tungstenite::connect_async(request).await?;
    Ok(stream)
}

/// Waits for the next *control* frame, skipping any binary output frames in
/// between -- a live shell can legitimately interleave a prompt or other
/// chatter with control messages, and every test in this file that isn't
/// specifically asserting on output framing wants to see past that.
async fn next_json(ws: &mut WsStream) -> Value {
    loop {
        match tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .expect("timed out waiting for a frame")
        {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).expect("valid JSON control frame")
            }
            Some(Ok(Message::Binary(_))) => continue,
            other => panic!("expected a text control frame, got {other:?}"),
        }
    }
}

/// Reads the next frame and asserts it is binary, returning `(offset,
/// bytes)` decoded per the wire format
/// (docs/04-api-protocol.md#framing: 8-byte BE offset, then raw bytes).
async fn next_binary(ws: &mut WsStream) -> (u64, Vec<u8>) {
    match tokio::time::timeout(TIMEOUT, ws.next())
        .await
        .expect("timed out waiting for a frame")
    {
        Some(Ok(Message::Binary(bytes))) => {
            assert!(
                bytes.len() >= 8,
                "binary frame shorter than the offset prefix"
            );
            let offset = u64::from_be_bytes(bytes[..8].try_into().unwrap());
            (offset, bytes[8..].to_vec())
        }
        other => panic!("expected a binary frame, got {other:?}"),
    }
}

async fn send_text(ws: &mut WsStream, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .expect("send");
}

#[tokio::test]
async fn ready_is_always_the_first_frame() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let url = daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=c1"));
    let mut ws = connect(&url, Some(support::TOKEN), None)
        .await
        .expect("connect");

    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    assert_eq!(ready["session_id"], id.to_string());
    assert_eq!(ready["control"], false, "no mode=control was requested");
}

#[tokio::test]
async fn mode_control_grants_the_lease_when_free() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let url = daemon.ws_url(&format!(
        "/api/v1/sessions/{id}/stream?client_id=c1&mode=control"
    ));
    let mut ws = connect(&url, Some(support::TOKEN), None)
        .await
        .expect("connect");
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["control"], true);
}

/// **The core control-lease invariant**
/// (docs/04-api-protocol.md#why-attach-must-not-preempt): a second client
/// attaching with `mode=control` while someone else holds the lease must
/// become an observer, never preempt.
#[tokio::test]
async fn mode_control_on_attach_does_not_preempt() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let mut first = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=a&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect first");
    let ready1 = next_json(&mut first).await;
    assert_eq!(ready1["control"], true);

    let mut second = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=b&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect second");
    let ready2 = next_json(&mut second).await;
    assert_eq!(
        ready2["control"], false,
        "attach must never preempt the current controller"
    );
    assert_eq!(ready2["controller"], "a");
}

/// `claim_control` always preempts, and the loser is told
/// (docs/04-api-protocol.md#control-lease).
#[tokio::test]
async fn claim_control_preempts_and_notifies_the_loser() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let mut a = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=a&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect a");
    let _ = next_json(&mut a).await; // ready

    let mut b = connect(
        &daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=b")),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect b");
    let _ = next_json(&mut b).await; // ready, observer

    send_text(&mut b, json!({ "type": "claim_control" })).await;
    let granted = next_json(&mut b).await;
    assert_eq!(granted["type"], "control_granted");

    let revoked = next_json(&mut a).await;
    assert_eq!(revoked["type"], "control_revoked");
    assert_eq!(revoked["client_id"], "b");
}

/// M4 review: the control lease used to be keyed purely on `client_id`, with
/// no way to tell apart two simultaneous connections that happen to share
/// one -- e.g. a reloaded tab racing its own not-yet-closed old socket. Both
/// used to pass as controller and could write concurrently. Now each grant
/// carries an epoch, and only the connection holding the *current* one
/// counts: the older of two same-`client_id` connections must be treated as
/// having lost control the moment the newer one attaches.
#[tokio::test]
async fn a_second_connection_sharing_a_client_id_supersedes_the_first() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let mut first = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=a&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect first");
    let ready1 = next_json(&mut first).await;
    assert_eq!(ready1["control"], true);

    // Same client_id, a second connection -- attach_control's
    // already-held-by-this-client_id branch grants it too.
    let mut second = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=a&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect second");
    let ready2 = next_json(&mut second).await;
    assert_eq!(ready2["control"], true);

    // The first connection's grant is now stale: its input must be rejected,
    // not silently reach the PTY alongside the second connection's.
    first
        .send(Message::Binary(b"echo hi\n".to_vec().into()))
        .await
        .expect("send input");
    let error = next_json(&mut first).await;
    assert_eq!(error["type"], "error");
    assert_eq!(
        error["code"], "not_controller",
        "a superseded same-client_id connection must not still be able to write"
    );
}

/// Input from an observer never reaches the PTY -- rejected with
/// `not_controller` instead (docs/10-testing.md#3-protocol-tests).
#[tokio::test]
async fn input_from_an_observer_is_rejected() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let mut ws = connect(
        &daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=observer")),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect");
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["control"], false);

    ws.send(Message::Binary(b"echo hi\n".to_vec().into()))
        .await
        .expect("send input");
    let error = next_json(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "not_controller");
}

/// Resize from an observer is rejected; resize from the controller reaches
/// every attached client as `resized`
/// (docs/04-api-protocol.md#control-messages).
#[tokio::test]
async fn resize_from_the_controller_reaches_observers_as_resized() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let mut controller = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=ctl&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect controller");
    let _ = next_json(&mut controller).await;

    let mut observer = connect(
        &daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=obs")),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect observer");
    let _ = next_json(&mut observer).await;

    // Rejected: the observer is not the controller.
    send_text(
        &mut observer,
        json!({ "type": "resize", "cols": 10, "rows": 10 }),
    )
    .await;
    let error = next_json(&mut observer).await;
    assert_eq!(error["code"], "not_controller");

    // Applied, and broadcast to the observer too.
    send_text(
        &mut controller,
        json!({ "type": "resize", "cols": 100, "rows": 50 }),
    )
    .await;
    let resized = next_json(&mut observer).await;
    assert_eq!(resized["type"], "resized");
    assert_eq!(resized["cols"], 100);
    assert_eq!(resized["rows"], 50);
}

/// The disconnect-grace / ping-pong pair from
/// docs/10-testing.md#3-protocol-tests: a controller that drops and
/// reconnects with the same `client_id` within `control_grace_ms` resumes
/// the lease; if someone else claims it first, the original reconnecting
/// with `mode=control` stays an observer.
#[tokio::test]
async fn disconnect_grace_resumes_for_the_same_client_but_never_wins_a_race() {
    let mut config = support::default_config();
    config.control_grace_ms = 2_000;
    let daemon = support::spawn(config).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let mut a = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=a&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect a");
    let _ = next_json(&mut a).await;
    drop(a); // ordinary disconnect, not release_control

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut a_again = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=a&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("reconnect a");
    let ready = next_json(&mut a_again).await;
    assert_eq!(
        ready["control"], true,
        "the same client_id must resume its lease inside the grace window"
    );
    drop(a_again);

    // "a" drops again, without reconnecting; "b" *claims* (an explicit human
    // action, always preempts -- docs/04-api-protocol.md#why-attach-must-not-preempt)
    // while "a" is still within its grace window.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut b = connect(
        &daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=b")),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect b");
    let _ = next_json(&mut b).await;
    send_text(&mut b, json!({ "type": "claim_control" })).await;
    let granted = next_json(&mut b).await;
    assert_eq!(granted["type"], "control_granted");

    // "a" reconnecting with mode=control must not win it back -- the
    // ping-pong regression test (docs/10-testing.md#3-protocol-tests).
    let mut a_third = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=a&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("reconnect a again");
    let ready3 = next_json(&mut a_third).await;
    assert_eq!(
        ready3["control"], false,
        "mode=control must never preempt whoever holds the lease now"
    );
    assert_eq!(ready3["controller"], "b");
}

/// The grace window expiring frees the lease -- and it is never auto-granted
/// to a waiting observer (docs/04-api-protocol.md#disconnect-grace).
#[tokio::test]
async fn grace_expiry_frees_the_lease_with_no_auto_grant() {
    let mut config = support::default_config();
    config.control_grace_ms = 300;
    let daemon = support::spawn(config).await;
    let id = support::create_shell_session(&daemon, vec![]);

    let mut a = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=a&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect a");
    let _ = next_json(&mut a).await;
    drop(a);

    tokio::time::sleep(Duration::from_millis(600)).await;

    let mut b = connect(
        &daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=b")),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect b");
    let ready = next_json(&mut b).await;
    assert_eq!(
        ready["control"], false,
        "b only observed -- proves the lease was not auto-granted to it"
    );
    assert!(
        ready["controller"].is_null(),
        "the lease must be free, not silently held by a's dead connection"
    );
}

/// Binary server frames carry the correct 8-byte BE offset, and consecutive
/// frames are contiguous (docs/04-api-protocol.md#framing).
#[tokio::test]
async fn binary_frames_carry_correct_contiguous_offsets() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(
        &daemon,
        vec!["-c".to_string(), "printf 'hello'".to_string()],
    );

    let mut ws = connect(
        &daemon.ws_url(&format!(
            "/api/v1/sessions/{id}/stream?client_id=c1&mode=control"
        )),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect");
    let ready = next_json(&mut ws).await;
    // The replay stretch (if any) comes first, starting at `replay_from` --
    // not `next_offset`, which is where the *live* stream picks up after it.
    let mut expected_offset = ready["replay_from"].as_u64().unwrap();

    let mut acc = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !acc.windows(5).any(|w| w == b"hello") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out; got {:?}",
            String::from_utf8_lossy(&acc)
        );
        let (offset, bytes) = next_binary(&mut ws).await;
        assert_eq!(offset, expected_offset, "offsets must be contiguous");
        expected_offset += bytes.len() as u64;
        acc.extend_from_slice(&bytes);
    }
}

/// The `exit` frame carries the final offset, matching `next_offset`
/// (docs/10-testing.md#3-protocol-tests).
#[tokio::test]
async fn exit_frame_carries_the_final_offset() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec!["-c".to_string(), "exit 7".to_string()]);

    let mut ws = connect(
        &daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=c1")),
        Some(support::TOKEN),
        None,
    )
    .await
    .expect("connect");
    let _ready = next_json(&mut ws).await;

    // Drain whatever output the shell produced before exiting (its own
    // startup chatter, if any) until the exit control frame arrives.
    loop {
        match tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .expect("timed out waiting for exit")
        {
            Some(Ok(Message::Binary(_))) => continue,
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(value["type"], "exit");
                assert_eq!(value["code"], 7);
                let session = daemon.state.sessions.get(id).expect("session still listed");
                assert_eq!(
                    value["final_offset"].as_u64().unwrap(),
                    session.next_offset()
                );
                break;
            }
            other => panic!("unexpected frame while waiting for exit: {other:?}"),
        }
    }
}

/// Origin/Host enforcement on the WS upgrade
/// (docs/06-security.md#browser-origin-defense).
#[tokio::test]
async fn bad_origin_is_rejected() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);
    let url = daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=c1"));
    let err = connect(&url, Some(support::TOKEN), Some("https://evil.example"))
        .await
        .unwrap_err();
    assert_handshake_rejected(err);
}

#[tokio::test]
async fn missing_origin_with_a_valid_credential_is_accepted() {
    // The native-client case: no Origin at all, but a valid bearer token.
    // Must be accepted -- asserting the opposite would block every future
    // mobile app (docs/10-testing.md#3-protocol-tests).
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);
    let url = daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=c1"));
    let mut ws = connect(&url, Some(support::TOKEN), None)
        .await
        .expect("connect");
    let ready = next_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
}

#[tokio::test]
async fn missing_origin_and_no_credential_is_rejected() {
    let daemon = support::spawn(support::default_config()).await;
    let id = support::create_shell_session(&daemon, vec![]);
    let url = daemon.ws_url(&format!("/api/v1/sessions/{id}/stream?client_id=c1"));
    let err = connect(&url, None, None).await.unwrap_err();
    assert_handshake_rejected(err);
}

fn assert_handshake_rejected(err: tokio_tungstenite::tungstenite::Error) {
    match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert!(
                response.status().is_client_error(),
                "expected a 4xx handshake rejection, got {}",
                response.status()
            );
        }
        other => panic!("expected an HTTP handshake rejection, got {other:?}"),
    }
}
