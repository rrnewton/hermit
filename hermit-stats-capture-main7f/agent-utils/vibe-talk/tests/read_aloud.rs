//! Device speech selection and the provider-independent read-aloud boundary.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use futures_util::{SinkExt as _, StreamExt as _, TryStreamExt as _};
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt as _;
use vibe_talk::config::{Config, ReadAloudBackend, ENV_READ_ALOUD_BACKEND};
use vibe_talk::http::router;
use vibe_talk::model::ChannelId;
use vibe_talk::speech::{
    ConversationSpeech, Description, Playback, SessionSetup, Speech, SpeechError, SpeechProvider,
};

/// A scripted `vibe-talk-v1` bridge. Every test counts its accepts, because "one accept" is the
/// observable meaning of "the read reused the session".
mod bridge {
    use futures_util::{SinkExt as _, StreamExt as _};
    use serde_json::{json, Value};
    use tokio_tungstenite::tungstenite::Message;

    pub type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    pub async fn listen() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let url = format!("ws://{}", listener.local_addr().expect("address"));
        (listener, url)
    }

    /// Accept one connection and announce the session.
    pub async fn accept(listener: &tokio::net::TcpListener) -> Socket {
        let (stream, _) = listener.accept().await.expect("accepts");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("upgrades");
        socket
            .send(Message::Text(r#"{"type":"session_started"}"#.into()))
            .await
            .expect("starts");
        socket
    }

    /// Prove nobody opens another connection: a reconnecting client would be waiting on one.
    pub async fn no_more_connections(listener: &tokio::net::TcpListener) {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), listener.accept())
                .await
                .is_err(),
            "the client opened a second connection"
        );
    }

    /// The next JSON frame the client sent, skipping keepalive control frames.
    pub async fn next_json(socket: &mut Socket) -> Value {
        loop {
            match socket.next().await.expect("a frame").expect("readable") {
                Message::Text(raw) => return serde_json::from_str(raw.as_ref()).expect("JSON"),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("unexpected client frame: {other:?}"),
            }
        }
    }

    /// Read a prompt and check which message it asks for.
    pub async fn expect_prompt(socket: &mut Socket, text: &str) {
        let frame = next_json(socket).await;
        assert_eq!(frame["type"], "prompt", "expected a prompt, got {frame}");
        let prompt = frame["text"].as_str().expect("prompt text");
        assert!(prompt.contains("Read the channel message"), "{prompt}");
        assert!(prompt.contains(text), "wrong prompt: {prompt}");
    }

    pub async fn expect_interrupt(socket: &mut Socket) {
        assert_eq!(
            next_json(socket).await,
            json!({"type": "interrupt"}),
            "the switch did not interrupt the running turn"
        );
    }

    pub async fn pcm(socket: &mut Socket, bytes: &[u8]) {
        socket
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .expect("audio");
    }

    pub async fn transcript(socket: &mut Socket, text: &str) {
        socket
            .send(Message::Text(
                json!({"type": "transcript", "role": "assistant", "text": text})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("transcript");
    }

    pub async fn complete(socket: &mut Socket, turn: u64, interrupted: bool) {
        let frame = if interrupted {
            json!({"type": "turn_complete", "turn": turn, "interrupted": true})
        } else {
            json!({"type": "turn_complete", "turn": turn})
        };
        socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .expect("turn completes");
    }

    pub async fn error(socket: &mut Socket, message: &str) {
        socket
            .send(Message::Text(
                json!({"type": "error", "message": message})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("error frame");
    }

    /// Speak one whole message: prompt, audio, completion.
    pub async fn serve(socket: &mut Socket, text: &str, audio: &[u8], turn: u64) {
        expect_prompt(socket, text).await;
        pcm(socket, audio).await;
        complete(socket, turn, false).await;
    }
}

fn provider(url: &str) -> ConversationSpeech {
    let config = vibe_talk::config::ConversationConfig {
        backend: vibe_talk::config::ConversationBackend::WebSocket,
        websocket_url: None,
        label: "Test agent".to_owned(),
        ask_before_send: true,
    };
    ConversationSpeech::new(&config, Some(url))
}

async fn collect(stream: vibe_talk::speech::SpeechStream) -> Vec<Vec<u8>> {
    stream
        .chunks
        .map_ok(|chunk| chunk.to_vec())
        .try_collect()
        .await
        .expect("streams")
}

#[tokio::test]
async fn conversational_read_aloud_drives_the_provider_neutral_socket_and_returns_wav() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut socket = bridge::accept(&listener).await;
        bridge::serve(&mut socket, "hello from the channel", &[1, 2, 3, 4], 1).await;
    });
    let config = vibe_talk::config::ConversationConfig {
        backend: vibe_talk::config::ConversationBackend::WebSocket,
        websocket_url: Some("ws://127.0.0.1:1/browser-route".to_owned()),
        label: "Test agent".to_owned(),
        ask_before_send: true,
    };
    let provider = ConversationSpeech::new(&config, Some(&url));
    assert_eq!(provider.describe().label, "Test agent");
    provider.warm_up().await.expect("warms a ready session");
    let spoken = provider
        .speak_stream("hello from the channel", None)
        .await
        .expect("speaks");
    assert_eq!(spoken.content_type, "audio/wav");
    let session = spoken.session.expect("session facts");
    assert!(session.reused, "the tap did not use the warmed session");
    assert_eq!(
        session.waited,
        SessionSetup::default(),
        "the tap waited for session setup that preparation had already done"
    );
    assert!(
        session.setup.connection > Duration::ZERO,
        "the warmed session's setup was not kept for the observation"
    );
    assert_eq!((session.generation, session.turn), (1, 1));
    assert_eq!(
        &spoken.preamble.as_ref().expect("WAV preamble")[..4],
        b"RIFF",
        "the container header was not separated from real audio"
    );
    assert_eq!(collect(spoken).await, vec![vec![1, 2, 3, 4]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn two_completed_reads_share_one_session_and_keep_their_audio_separate() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut socket = bridge::accept(&listener).await;
        bridge::serve(&mut socket, "first distinct message", &[1, 1], 1).await;
        bridge::serve(&mut socket, "second distinct message", &[2, 2], 2).await;
        bridge::no_more_connections(&listener).await;
    });
    let provider = provider(&url);
    provider.warm_up().await.expect("warms one session");

    let first = provider
        .speak_stream_for_selection("first distinct message", None, Some("tap-a"))
        .await
        .expect("first read");
    let first_session = first.session.expect("first session");
    assert_eq!(collect(first).await, vec![vec![1, 1]]);
    let second = provider
        .speak_stream_for_selection("second distinct message", None, Some("tap-b"))
        .await
        .expect("second read");
    let second_session = second.session.expect("second session");
    assert_eq!(collect(second).await, vec![vec![2, 2]]);
    assert_eq!(first_session.generation, second_session.generation);
    assert_eq!((first_session.turn, second_session.turn), (1, 2));
    assert!(second_session.reused && !second_session.retried);
    assert_eq!(second_session.waited, SessionSetup::default());
    server.await.expect("one session served both reads");
}

#[tokio::test]
async fn switching_mid_turn_interrupts_and_begins_the_next_read_on_the_same_session() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        // ONE accept: the interrupted read and its replacement must share this session.
        let mut socket = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut socket, "interrupt this message").await;
        bridge::pcm(&mut socket, &[3, 3]).await;
        bridge::transcript(&mut socket, "interrupt this").await;
        bridge::expect_interrupt(&mut socket).await;
        // Output racing with the interrupt still belongs to turn one and must be discarded.
        bridge::pcm(&mut socket, &[9, 9]).await;
        bridge::transcript(&mut socket, "message, late").await;
        bridge::complete(&mut socket, 1, true).await;
        bridge::serve(&mut socket, "clean second message", &[4, 4], 2).await;
        bridge::no_more_connections(&listener).await;
    });
    let provider = provider(&url);

    let mut first = provider
        .speak_stream_for_selection("interrupt this message", None, Some("tap-a"))
        .await
        .expect("starts the first read");
    let first_session = first.session.expect("first session");
    assert_eq!(
        &first.chunks.next().await.expect("a chunk").expect("audio")[..],
        &[3, 3]
    );
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        provider.speak_stream_for_selection("clean second message", None, Some("tap-b")),
    )
    .await
    .expect("the second read did not begin promptly")
    .expect("reuses the interrupted session");
    let second_session = second.session.expect("second session");
    let cut = first
        .chunks
        .next()
        .await
        .expect("the interrupted read ended silently, as if it were complete")
        .expect_err("the interrupted read kept streaming after the switch");
    assert_eq!(cut.code(), "conversation_speech_superseded");
    assert!(first.chunks.next().await.is_none());
    assert_eq!(
        collect(second).await,
        vec![vec![4, 4]],
        "late first-turn audio bled into the second read"
    );
    assert_eq!(
        first_session.generation, second_session.generation,
        "the switch replaced the session"
    );
    assert_eq!((first_session.turn, second_session.turn), (1, 2));
    assert!(second_session.reused && !second_session.retried);
    assert_eq!(second_session.waited, SessionSetup::default());

    // A late request for the superseded selection must not cut off its successor.
    let late = provider
        .speak_stream_for_selection("interrupt this message", None, Some("tap-a"))
        .await
        .expect_err("a superseded selection was read again");
    assert_eq!(late.code(), "conversation_speech_superseded");
    server.await.expect("one session served the switch");
}

#[tokio::test]
async fn a_cancelled_read_is_interrupted_and_the_session_stays_usable() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut socket = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut socket, "a read the reader stops").await;
        bridge::pcm(&mut socket, &[5, 5]).await;
        // The response body going away (the reader pressed stop) is the interrupt signal.
        bridge::expect_interrupt(&mut socket).await;
        bridge::pcm(&mut socket, &[9, 9]).await;
        bridge::complete(&mut socket, 1, true).await;
        bridge::serve(&mut socket, "the next message", &[6, 6], 2).await;
        bridge::no_more_connections(&listener).await;
    });
    let provider = provider(&url);
    let mut first = provider
        .speak_stream("a read the reader stops", None)
        .await
        .expect("first read");
    assert_eq!(
        &first.chunks.next().await.expect("a chunk").expect("audio")[..],
        &[5, 5]
    );
    drop(first);
    let second = provider
        .speak_stream("the next message", None)
        .await
        .expect("second read");
    let session = second.session.expect("session");
    assert_eq!((session.generation, session.turn), (1, 2));
    assert_eq!(collect(second).await, vec![vec![6, 6]]);
    server.await.expect("one session served both");
}

#[tokio::test]
async fn parts_of_one_selection_queue_instead_of_interrupting_each_other() {
    let (listener, url) = bridge::listen().await;
    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut socket = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut socket, "first part").await;
        bridge::pcm(&mut socket, &[1]).await;
        queued_rx.await.expect("second part requested");
        bridge::pcm(&mut socket, &[1]).await;
        bridge::complete(&mut socket, 1, false).await;
        // The very next frame is the second part's prompt, not an interrupt.
        bridge::serve(&mut socket, "second part", &[2], 2).await;
    });
    let provider = provider(&url);
    let first = provider
        .speak_stream_for_selection("first part", None, Some("row"))
        .await
        .expect("first part");
    let second = tokio::spawn({
        let provider = provider.clone();
        async move {
            provider
                .speak_stream_for_selection("second part", None, Some("row"))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    queued_tx.send(()).expect("server waiting");
    assert_eq!(collect(first).await, vec![vec![1], vec![1]]);
    let second = second.await.expect("joins").expect("second part");
    assert_eq!(second.session.expect("session").turn, 2);
    assert_eq!(collect(second).await, vec![vec![2]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn an_old_bridge_that_rejects_interrupt_is_replaced_for_the_next_read() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut old = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut old, "first message").await;
        bridge::pcm(&mut old, &[1, 1]).await;
        bridge::expect_interrupt(&mut old).await;
        // What a bridge that predates `interrupt` answers, leaving the socket open.
        bridge::error(&mut old, "unknown variant `interrupt`").await;
        bridge::pcm(&mut old, &[9, 9]).await;
        let mut fresh = bridge::accept(&listener).await;
        bridge::serve(&mut fresh, "second message", &[2, 2], 1).await;
        drop(old);
    });
    let provider = provider(&url);
    let mut first = provider
        .speak_stream_for_selection("first message", None, Some("tap-a"))
        .await
        .expect("first read");
    assert!(first.chunks.next().await.is_some());
    let second = tokio::time::timeout(
        Duration::from_secs(2),
        provider.speak_stream_for_selection("second message", None, Some("tap-b")),
    )
    .await
    .expect("the switch waited out the interrupt timeout instead of reacting to the error")
    .expect("second read on a fresh session");
    let session = second.session.expect("session");
    assert_eq!((session.generation, session.turn), (2, 1));
    assert!(session.retried && !session.reused);
    assert_eq!(collect(second).await, vec![vec![2, 2]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn an_unacknowledged_interrupt_is_bounded_and_the_next_read_reconnects() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut silent = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut silent, "first message").await;
        bridge::pcm(&mut silent, &[1, 1]).await;
        bridge::expect_interrupt(&mut silent).await;
        // Never acknowledged.
        let mut fresh = bridge::accept(&listener).await;
        bridge::serve(&mut fresh, "second message", &[2, 2], 1).await;
        drop(silent);
    });
    let provider = provider(&url);
    let mut first = provider
        .speak_stream_for_selection("first message", None, Some("tap-a"))
        .await
        .expect("first read");
    assert!(first.chunks.next().await.is_some());
    let second = tokio::time::timeout(
        Duration::from_secs(6),
        provider.speak_stream_for_selection("second message", None, Some("tap-b")),
    )
    .await
    .expect("the unacknowledged interrupt was not bounded")
    .expect("second read on a fresh session");
    let session = second.session.expect("session");
    assert!(
        session.yielded >= Duration::from_secs(2),
        "yield time did not account for the missing ack: {:?}",
        session.yielded
    );
    assert_eq!(collect(second).await, vec![vec![2, 2]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn a_socket_that_accepts_the_prompt_and_then_drops_is_retried_once() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut first = bridge::accept(&listener).await;
        // The write succeeded; the read of any reply fails.
        bridge::expect_prompt(&mut first, "retry this prompt").await;
        drop(first);
        let mut second = bridge::accept(&listener).await;
        bridge::serve(&mut second, "retry this prompt", &[9, 8, 7, 6], 1).await;
    });
    let provider = provider(&url);
    provider.warm_up().await.expect("warms the first session");
    let spoken = provider
        .speak_stream("retry this prompt", None)
        .await
        .expect("retries");
    let session = spoken.session.expect("session");
    assert!(session.retried, "the retry was not reported");
    assert_eq!(session.generation, 2);
    assert!(
        session.waited.connection > Duration::ZERO,
        "the replacement connection was not attributed to the playback request"
    );
    assert_eq!(collect(spoken).await, vec![vec![9, 8, 7, 6]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn a_queued_protocol_error_is_recovered_on_a_fresh_session() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut first = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut first, "recover this prompt").await;
        bridge::error(&mut first, "the provider lost the session").await;
        let mut second = bridge::accept(&listener).await;
        bridge::serve(&mut second, "recover this prompt", &[4, 3], 1).await;
        drop(first);
    });
    let provider = provider(&url);
    provider.warm_up().await.expect("warms the first session");
    let spoken = provider
        .speak_stream("recover this prompt", None)
        .await
        .expect("recovers");
    assert!(spoken.session.expect("session").retried);
    assert_eq!(collect(spoken).await, vec![vec![4, 3]]);
    server.await.expect("server exits");
}

/// The first-audio watchdog used by the tests below, in place of the 15-second default.
const WATCHDOG: Duration = Duration::from_millis(300);

#[tokio::test]
async fn a_switched_read_whose_answer_never_arrives_is_retried_by_the_first_audio_watchdog() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut dropped = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut dropped, "first message").await;
        bridge::pcm(&mut dropped, &[1, 1]).await;
        bridge::expect_interrupt(&mut dropped).await;
        bridge::complete(&mut dropped, 1, true).await;
        bridge::expect_prompt(&mut dropped, "second message").await;
        // B's error (or its whole reply) arrived before B's turn started, and the bridge fenced
        // it off as stale output from A: nothing more is sent for B. The socket stays open and
        // readable, so only a watchdog on the prompt can notice.
        assert_eq!(
            bridge::next_json(&mut dropped).await,
            json!({"type": "quit"}),
            "the watchdog did not close the unanswered session"
        );
        let mut fresh = bridge::accept(&listener).await;
        bridge::serve(&mut fresh, "second message", &[2, 2], 1).await;
        bridge::no_more_connections(&listener).await;
    });
    let provider = provider(&url).with_first_audio_timeout(WATCHDOG);
    let mut first = provider
        .speak_stream_for_selection("first message", None, Some("tap-a"))
        .await
        .expect("first read");
    assert!(first.chunks.next().await.is_some());
    let switched = std::time::Instant::now();
    let second = tokio::time::timeout(
        WATCHDOG + Duration::from_secs(2),
        provider.speak_stream_for_selection("second message", None, Some("tap-b")),
    )
    .await
    .expect("the unanswered prompt was not bounded by the first-audio watchdog")
    .expect("second read retried on a fresh session");
    assert!(
        switched.elapsed() >= WATCHDOG,
        "retried before the watchdog"
    );
    let session = second.session.expect("session");
    assert_eq!((session.generation, session.turn), (2, 1));
    assert!(session.retried && !session.reused);
    assert_eq!(collect(second).await, vec![vec![2, 2]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn a_switched_read_whose_turn_never_starts_is_retried_on_the_servers_error_and_close() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut fenced = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut fenced, "first message").await;
        bridge::pcm(&mut fenced, &[1, 1]).await;
        bridge::expect_interrupt(&mut fenced).await;
        bridge::complete(&mut fenced, 1, true).await;
        bridge::expect_prompt(&mut fenced, "second message").await;
        // A server's own watchdog for a turn that never started after an interrupt: an explicit
        // error, then the socket closes.
        bridge::error(
            &mut fenced,
            "turn 2 never started after an interrupt; reconnect",
        )
        .await;
        fenced.close(None).await.expect("closes");
        drop(fenced);
        let mut fresh = bridge::accept(&listener).await;
        bridge::serve(&mut fresh, "second message", &[2, 2], 1).await;
        bridge::no_more_connections(&listener).await;
    });
    // The default first-audio watchdog, so only the server's error can explain a prompt retry.
    let provider = provider(&url);
    let mut first = provider
        .speak_stream_for_selection("first message", None, Some("tap-a"))
        .await
        .expect("first read");
    assert!(first.chunks.next().await.is_some());
    let second = tokio::time::timeout(
        Duration::from_secs(2),
        provider.speak_stream_for_selection("second message", None, Some("tap-b")),
    )
    .await
    .expect("the server's error did not end the unanswered read before the client watchdog")
    .expect("second read retried on a fresh session");
    let session = second.session.expect("session");
    assert_eq!((session.generation, session.turn), (2, 1));
    assert!(session.retried && !session.reused);
    assert_eq!(collect(second).await, vec![vec![2, 2]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn a_prompt_that_never_gets_audio_fails_within_two_first_audio_watchdogs() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let mut silent = bridge::accept(&listener).await;
            bridge::expect_prompt(&mut silent, "unanswered message").await;
            assert_eq!(
                bridge::next_json(&mut silent).await,
                json!({"type": "quit"})
            );
        }
        bridge::no_more_connections(&listener).await;
    });
    let provider = provider(&url).with_first_audio_timeout(WATCHDOG);
    let asked = std::time::Instant::now();
    let error = tokio::time::timeout(
        WATCHDOG * 2 + Duration::from_secs(2),
        provider.speak_stream("unanswered message", None),
    )
    .await
    .expect("an unanswered prompt hung")
    .expect_err("an unanswered prompt succeeded");
    assert!(asked.elapsed() >= WATCHDOG * 2, "gave up before retrying");
    assert_eq!(error.code(), "conversation_speech_timeout");
    server.await.expect("server exits");
}

#[tokio::test]
async fn conversational_read_aloud_reconnects_when_the_warmed_socket_was_reset() {
    #[allow(deprecated)]
    fn abort_on_drop(stream: &tokio::net::TcpStream) {
        // Test-only negative control. A zero linger duration makes the close an immediate reset;
        // it cannot block, which is the hazard behind the API's general deprecation.
        stream
            .set_linger(Some(Duration::ZERO))
            .expect("sets abortive close");
    }

    let (listener, url) = bridge::listen().await;
    let (stale_tx, stale_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let first = bridge::accept(&listener).await;
        abort_on_drop(first.get_ref());
        drop(first);
        stale_tx.send(()).expect("reports stale socket");
        let mut second = bridge::accept(&listener).await;
        bridge::serve(&mut second, "retry this prompt", &[9, 8, 7, 6], 1).await;
    });
    let provider = provider(&url);
    provider.warm_up().await.expect("warms the first session");
    stale_rx.await.expect("first session became stale");
    let spoken = provider
        .speak_stream("retry this prompt", None)
        .await
        .expect("reconnects");
    let session = spoken.session.expect("session");
    assert!(!session.reused, "a reset session was reported as reused");
    assert!(session.waited.connection > Duration::ZERO);
    assert_eq!(collect(spoken).await, vec![vec![9, 8, 7, 6]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn warm_up_replaces_a_session_the_provider_has_closed() {
    let (listener, url) = bridge::listen().await;
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut first = bridge::accept(&listener).await;
        first
            .send(Message::Close(None))
            .await
            .expect("closes the session");
        // Wait for the client's side of the close, so it has certainly noticed.
        while let Some(Ok(_)) = first.next().await {}
        closed_tx.send(()).expect("reports the close");
        let mut second = bridge::accept(&listener).await;
        bridge::serve(&mut second, "after the replacement", &[5, 4, 3, 2], 1).await;
        bridge::no_more_connections(&listener).await;
    });
    let provider = provider(&url);
    provider.warm_up().await.expect("warms the first session");
    closed_rx.await.expect("the provider closed it");
    provider
        .warm_up()
        .await
        .expect("preparation opens a replacement");
    let spoken = provider
        .speak_stream("after the replacement", None)
        .await
        .expect("reads on the replacement");
    let session = spoken.session.expect("session");
    assert!(
        session.reused,
        "the tap paid for setup preparation should have done"
    );
    assert_eq!((session.generation, session.turn), (2, 1));
    assert_eq!(collect(spoken).await, vec![vec![5, 4, 3, 2]]);
    server.await.expect("server exits");
}

#[tokio::test]
async fn the_buffered_fallback_reuses_the_session_too() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut socket = bridge::accept(&listener).await;
        bridge::serve(&mut socket, "first buffered", &[1, 0], 1).await;
        bridge::serve(&mut socket, "second buffered", &[2, 0], 2).await;
        bridge::no_more_connections(&listener).await;
    });
    let provider = provider(&url);
    for (text, audio) in [("first buffered", [1_u8, 0]), ("second buffered", [2, 0])] {
        let spoken = provider.speak(text, None).await.expect("buffered read");
        assert_eq!(spoken.content_type, "audio/wav");
        assert_eq!(&spoken.audio[..4], b"RIFF");
        assert_eq!(&spoken.audio[44..], &audio);
    }
    server
        .await
        .expect("one session served both buffered reads");
}

#[tokio::test]
async fn a_switch_through_the_audio_route_reports_the_reused_session() {
    let (listener, url) = bridge::listen().await;
    let server = tokio::spawn(async move {
        let mut socket = bridge::accept(&listener).await;
        bridge::expect_prompt(&mut socket, "First routed message").await;
        bridge::pcm(&mut socket, &[1, 1]).await;
        bridge::expect_interrupt(&mut socket).await;
        bridge::complete(&mut socket, 1, true).await;
        bridge::serve(&mut socket, "Second routed message", &[2, 2], 2).await;
        bridge::no_more_connections(&listener).await;
    });
    let (mut state, chat, _elevenlabs) =
        testing::state_from_toml(&testing::config_toml_without_elevenlabs());
    state.speech = Arc::new(provider(&url));
    let channel = ChannelId(READ_CHANNEL.to_owned());
    let first = chat.seed(&channel, "Reader", "First routed message");
    let second = chat.seed(&channel, "Reader", "Second routed message");
    let app = router(state);
    let (status, bytes) = call(
        &app,
        "POST",
        &format!("/api/v1/channels/{READ_CHANNEL}/speech/prepare"),
        Some(json!({"ids": [first.0, second.0]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let prepared: Value = serde_json::from_slice(&bytes).expect("prepared");
    let url_for = |id: &str| {
        prepared["prepared"]
            .as_array()
            .expect("prepared list")
            .iter()
            .find(|entry| entry["message_id"] == id)
            .and_then(|entry| entry["url"].as_str())
            .expect("prepared url")
            .to_owned()
    };

    let (status, _headers, _bytes) = call_with_headers(
        &app,
        "GET",
        &format!("{}?selection=not%20valid", url_for(&first.0)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let request = Request::builder()
        .uri(format!("{}?selection=tap-a", url_for(&first.0)))
        .body(Body::empty())
        .expect("request");
    let first_response = app.clone().oneshot(request).await.expect("router");
    assert_eq!(first_response.status(), StatusCode::OK);
    let mut first_body = first_response.into_body();
    let preamble = first_body.frame().await.expect("frame").expect("bytes");
    assert_eq!(&preamble.into_data().expect("data")[..4], b"RIFF");

    let (status, headers, bytes) = call_with_headers(
        &app,
        "GET",
        &format!("{}?selection=tap-b", url_for(&second.0)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[44..], &[2, 2], "the first read's audio bled in");
    let timing = headers["server-timing"].to_str().expect("server timing");
    assert!(
        timing.contains(r#"session;desc="generation=1 turn=2 reused=true""#),
        "{timing}"
    );
    assert!(timing.contains("request_connect;dur=0"), "{timing}");
    server.await.expect("one session served both taps");
}

use vibe_talk::testing::{self, READ_CHANNEL, READ_TOKEN};

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    payload: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let (status, _headers, bytes) = call_with_headers(app, method, uri, payload).await;
    (status, bytes)
}

async fn call_with_headers(
    app: &axum::Router,
    method: &str,
    uri: &str,
    payload: Option<Value>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {READ_TOKEN}"))
        .header("content-type", "application/json")
        .body(payload.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("router");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, headers, bytes.to_vec())
}

#[test]
fn browser_selection_is_explicit_and_needs_no_elevenlabs_settings() {
    let base = testing::config_toml_without_elevenlabs();
    let default = Config::from_toml_and_env(&base, &BTreeMap::new()).expect("default config");
    assert_eq!(default.read_aloud.backend, ReadAloudBackend::ElevenLabs);

    let browser = format!("{base}\n[read_aloud]\nbackend = 'browser'\n");
    let config = Config::from_toml_and_env(&browser, &BTreeMap::new()).expect("browser config");
    assert_eq!(config.read_aloud.backend, ReadAloudBackend::Browser);
    assert!(config.elevenlabs.api_key.is_none());
    assert!(config.elevenlabs.agent_id.is_none());

    let overridden = Config::from_toml_and_env(
        &browser,
        &BTreeMap::from([(ENV_READ_ALOUD_BACKEND.to_owned(), "elevenlabs".to_owned())]),
    )
    .expect("environment selection");
    assert_eq!(overridden.read_aloud.backend, ReadAloudBackend::ElevenLabs);

    for invalid in ["cloud", "browsre", ""] {
        let text = format!("{base}\n[read_aloud]\nbackend = '{invalid}'\n");
        assert!(Config::from_toml_and_env(&text, &BTreeMap::new()).is_err());
    }
    assert!(Config::from_toml_and_env(
        &base,
        &BTreeMap::from([(ENV_READ_ALOUD_BACKEND.to_owned(), "cloud".to_owned())]),
    )
    .is_err());
}

#[tokio::test]
async fn browser_config_advertises_device_playback_and_never_uses_elevenlabs() {
    let text = format!(
        "{}\n[read_aloud]\nbackend = 'browser'\n",
        testing::config_toml_without_elevenlabs()
    );
    let (state, chat, elevenlabs) = testing::state_from_toml(&text);
    let id = chat.seed(
        &ChannelId(READ_CHANNEL.to_owned()),
        "Reader",
        "Read these words",
    );
    let app = router(state);
    let (status, bytes) = call(&app, "GET", "/api/v1/client-config", None).await;
    assert_eq!(status, StatusCode::OK);
    let config: Value = serde_json::from_slice(&bytes).expect("config");
    assert_eq!(
        config["read_aloud"],
        json!({"backend": "browser", "label": "Device voice", "playback": "browser", "local_only": true})
    );
    assert_eq!(config["elevenlabs_agent_id"], Value::Null);

    let (status, bytes) = call(
        &app,
        "GET",
        &format!("/api/v1/channels/{READ_CHANNEL}/messages"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&bytes).contains("Read these words"));

    // A stale browser that still calls the audio routes receives an actionable answer, rather
    // than spending a vendor request or being told to configure credentials it does not need.
    for (uri, body) in [
        (
            format!("/api/v1/channels/{READ_CHANNEL}/messages/{id}/speak"),
            None,
        ),
        (
            format!("/api/v1/channels/{READ_CHANNEL}/speech/prepare"),
            Some(json!({"ids": [id.0]})),
        ),
    ] {
        let (status, bytes) = call(&app, "POST", &uri, body).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let error: Value = serde_json::from_slice(&bytes).expect("error");
        assert_eq!(error["error"], "browser_speech_required");
        assert!(!String::from_utf8_lossy(&bytes)
            .to_lowercase()
            .contains("elevenlabs"));
    }
    assert!(elevenlabs.requested().is_empty());
    assert!(elevenlabs.spoken().is_empty());
    assert!(elevenlabs.chats().is_empty());
}

struct IndependentSpeech;

#[async_trait::async_trait]
impl SpeechProvider for IndependentSpeech {
    fn describe(&self) -> Description {
        Description {
            backend: "independent",
            label: "Independent voice".to_owned(),
            playback: Playback::Audio,
            local_only: false,
        }
    }

    async fn speak(&self, text: &str, _speed: Option<f64>) -> Result<Speech, SpeechError> {
        assert!(
            !text.is_empty(),
            "warming a provider must not synthesize empty text"
        );
        Ok(Speech {
            audio: text.as_bytes().to_vec(),
            content_type: "audio/test".to_owned(),
        })
    }
}

struct SlowWarmSpeech;

/// Far longer than a preparation takes, so a prepare that waited for it cannot be missed.
const SLOW_WARM_UP: Duration = Duration::from_secs(2);

#[async_trait::async_trait]
impl SpeechProvider for SlowWarmSpeech {
    fn describe(&self) -> Description {
        Description {
            backend: "timed-test",
            label: "Timed test voice".to_owned(),
            playback: Playback::Audio,
            local_only: false,
        }
    }

    async fn warm_up(&self) -> Result<(), SpeechError> {
        tokio::time::sleep(SLOW_WARM_UP).await;
        Ok(())
    }

    async fn speak(&self, text: &str, _speed: Option<f64>) -> Result<Speech, SpeechError> {
        Ok(Speech {
            audio: text.as_bytes().to_vec(),
            content_type: "audio/test".to_owned(),
        })
    }
}

/// `#191 read-aloud-first-lag`: the warm-up runs BESIDE the preparation, so neither the prepare
/// response nor the message-preparation phase it reports waits for it. (That only one warm-up runs
/// at a time is in `tests/read_ahead.rs`.)
#[tokio::test]
async fn message_preparation_neither_waits_for_nor_includes_provider_warm_up() {
    let (mut state, chat, _provider) =
        testing::state_from_toml(&testing::config_toml_without_elevenlabs());
    state.speech = Arc::new(SlowWarmSpeech);
    let id = chat.seed(
        &ChannelId(READ_CHANNEL.to_owned()),
        "Reader",
        "A timing-only test message",
    );
    let app = router(state);
    let began = std::time::Instant::now();
    let (status, bytes) = call(
        &app,
        "POST",
        &format!("/api/v1/channels/{READ_CHANNEL}/speech/prepare"),
        Some(json!({"ids": [id.0]})),
    )
    .await;
    let preparation_request_ms = began.elapsed().as_millis();
    assert_eq!(status, StatusCode::OK);
    assert!(
        preparation_request_ms < SLOW_WARM_UP.as_millis() / 2,
        "the prepare took {preparation_request_ms} ms, so it waited for the {SLOW_WARM_UP:?} warm-up"
    );
    let prepared: Value = serde_json::from_slice(&bytes).expect("prepared response");
    let url = prepared["prepared"][0]["url"].as_str().expect("ticket");
    let (status, headers, _bytes) = call_with_headers(&app, "GET", url, None).await;
    assert_eq!(status, StatusCode::OK);
    let timing = headers["server-timing"].to_str().expect("server timing");
    let message_preparation_ms: u128 = timing
        .split(',')
        .find_map(|part| part.trim().strip_prefix("message_prepare;dur="))
        .expect("message preparation phase")
        .parse()
        .expect("numeric duration");
    assert!(
        message_preparation_ms <= preparation_request_ms,
        "message preparation ({message_preparation_ms} ms) outlasted its own {preparation_request_ms} ms request"
    );
}

#[tokio::test]
async fn audio_routes_accept_a_provider_that_has_no_elevenlabs_configuration() {
    let (mut state, chat, elevenlabs) =
        testing::state_from_toml(&testing::config_toml_without_elevenlabs());
    state.speech = Arc::new(IndependentSpeech);
    let id = chat.seed(
        &ChannelId(READ_CHANNEL.to_owned()),
        "Reader",
        "**Speak** this",
    );
    let app = router(state);
    let (status, bytes) = call(&app, "GET", "/api/v1/client-config", None).await;
    assert_eq!(status, StatusCode::OK);
    let config: Value = serde_json::from_slice(&bytes).expect("config");
    assert_eq!(config["read_aloud"]["backend"], "independent");
    assert_eq!(config["read_aloud"]["playback"], "audio");

    let (status, bytes) = call(
        &app,
        "POST",
        &format!("/api/v1/channels/{READ_CHANNEL}/messages/{id}/speak"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8(bytes).expect("test audio"), "Speak this");

    let (status, bytes) = call(
        &app,
        "POST",
        &format!("/api/v1/channels/{READ_CHANNEL}/speech/prepare"),
        Some(json!({"ids": [id.0]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let prepared: Value = serde_json::from_slice(&bytes).expect("prepared");
    let url = prepared["prepared"][0]["url"].as_str().expect("ticket");
    let (status, bytes) = call(&app, "GET", url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        String::from_utf8(bytes).expect("stream audio"),
        "Speak this"
    );
    assert!(elevenlabs.requested().is_empty());
    assert!(elevenlabs.spoken().is_empty());
}

#[tokio::test]
async fn elevenlabs_audio_failures_keep_their_codes_status_and_redaction() {
    const REJECTED_KEY: &str = "xi-rejected-test-key";
    let config =
        testing::config_toml().replace(vibe_talk::elevenlabs::fake::VALID_API_KEY, REJECTED_KEY);
    let (state, chat, _elevenlabs) = testing::state_from_toml(&config);
    let id = chat.seed(
        &ChannelId(READ_CHANNEL.to_owned()),
        "Reader",
        "Read these words",
    );
    let app = router(state);
    let (status, bytes) = call(&app, "GET", "/api/v1/client-config", None).await;
    assert_eq!(status, StatusCode::OK);
    let config: Value = serde_json::from_slice(&bytes).expect("config");
    assert_eq!(config["read_aloud"]["backend"], "elevenlabs");
    assert_eq!(config["read_aloud"]["playback"], "audio");

    let (status, bytes) = call(
        &app,
        "POST",
        &format!("/api/v1/channels/{READ_CHANNEL}/speech/prepare"),
        Some(json!({"ids": [id.0]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let prepared: Value = serde_json::from_slice(&bytes).expect("prepared");
    let url = prepared["prepared"][0]["url"].as_str().expect("ticket");

    for (method, uri) in [
        ("GET", url.to_owned()),
        (
            "POST",
            format!("/api/v1/channels/{READ_CHANNEL}/messages/{id}/speak"),
        ),
    ] {
        let (status, bytes) = call(&app, method, &uri, None).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let error: Value = serde_json::from_slice(&bytes).expect("error");
        assert_eq!(error["error"], "elevenlabs_error");
        assert!(error["detail"]
            .as_str()
            .expect("detail")
            .contains("HTTP 401"));
        assert!(!String::from_utf8_lossy(&bytes).contains(REJECTED_KEY));
    }
}
