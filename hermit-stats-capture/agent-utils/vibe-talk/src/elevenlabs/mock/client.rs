//! A small conversation client, for whoever is driving the mock.
//!
//! The browser is the real client; this is what a test, or the mock binary's own self-check,
//! uses to stand in for it. It is deliberately literal — it sends the same events `web/voice.js`
//! sends, in the same shapes — so that a change which breaks the page breaks this too.
//!
//! Every read has a deadline. A conversation that stops answering is one of the conditions this
//! mock exists to produce ([`super::Scenario::NoReply`]), and a client that hung waiting for it
//! would turn a fast, clear failure into a suite that never finishes.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::audio;

/// How long a read waits before giving up, when the caller does not say.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why the conversation client could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The socket could not be opened, or the server refused the upgrade.
    #[error("could not open the conversation: {0}")]
    Connect(String),
    /// The socket failed after it was open.
    #[error("the conversation socket failed: {0}")]
    Socket(String),
    /// Nothing arrived inside the deadline.
    #[error("waited {0:?} and the agent said nothing")]
    Silent(Duration),
}

/// One event read off the socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    /// A JSON application event, with its `type` already pulled out.
    Event {
        /// The `type` field, or the empty string when there was none.
        kind: String,
        /// The whole event.
        value: Value,
    },
    /// The server closed. Carries the code a browser would report.
    Closed {
        /// RFC6455 close code, or `1005` when the peer gave none — which is what a browser
        /// reports for a close frame with no status.
        code: u16,
        /// The close reason, which the mock uses to name the refusal.
        reason: String,
    },
    /// The connection went away with no close frame at all: a browser reports `1006` here.
    Dropped,
}

impl Incoming {
    /// The event's `type`, or `None` for a close.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        match self {
            Self::Event { kind, .. } => Some(kind),
            Self::Closed { .. } | Self::Dropped => None,
        }
    }
}

/// A live conversation with the mock.
#[derive(Debug)]
pub struct ConversationClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl ConversationClient {
    /// Dial a minted signed URL.
    ///
    /// # Errors
    ///
    /// [`ClientError::Connect`] when the TCP connection fails or the server answers the upgrade
    /// with anything but 101 — which is exactly what a spent or expired nonce produces.
    pub async fn connect(signed_url: &str) -> Result<Self, ClientError> {
        let (socket, _response) = tokio_tungstenite::connect_async(signed_url)
            .await
            .map_err(|e| ClientError::Connect(e.to_string()))?;
        Ok(Self { socket })
    }

    /// Send the initiation the vendor requires before anything else.
    ///
    /// # Errors
    ///
    /// [`ClientError::Socket`] when the frame cannot be written.
    pub async fn initiate(&mut self) -> Result<(), ClientError> {
        self.send(&json!({ "type": "conversation_initiation_client_data" }))
            .await
    }

    /// Send any client event verbatim.
    ///
    /// # Errors
    ///
    /// [`ClientError::Socket`] when the frame cannot be written.
    pub async fn send(&mut self, event: &Value) -> Result<(), ClientError> {
        self.socket
            .send(Message::Text(event.to_string().into()))
            .await
            .map_err(|e| ClientError::Socket(e.to_string()))?;
        self.socket
            .flush()
            .await
            .map_err(|e| ClientError::Socket(e.to_string()))
    }

    /// Ask a question in text, the way the smoke script does.
    ///
    /// # Errors
    ///
    /// [`ClientError::Socket`] when the frame cannot be written.
    pub async fn ask(&mut self, text: &str) -> Result<(), ClientError> {
        self.send(&json!({ "type": "user_message", "text": text }))
            .await
    }

    /// Upload one chunk of PCM, in the shape `web/voice.js` uploads it.
    ///
    /// # Errors
    ///
    /// [`ClientError::Socket`] when the frame cannot be written.
    pub async fn send_audio(&mut self, pcm: &[u8]) -> Result<(), ClientError> {
        self.send(&json!({ "user_audio_chunk": audio::encode(pcm) }))
            .await
    }

    /// Answer a JSON `ping` event with the `pong` it is waiting for.
    ///
    /// # Errors
    ///
    /// [`ClientError::Socket`] when the frame cannot be written.
    pub async fn pong(&mut self, ping: &Value) -> Result<(), ClientError> {
        let id = ping["ping_event"]["event_id"].clone();
        self.send(&json!({ "type": "pong", "event_id": id })).await
    }

    /// Read the next event, waiting at most [`DEFAULT_TIMEOUT`].
    ///
    /// This does NOT answer pings. Answering them here would make it impossible to test that an
    /// unanswered ping is noticed, which is one of the conditions worth knowing about.
    ///
    /// # Errors
    ///
    /// [`ClientError::Silent`] when nothing arrives in time, [`ClientError::Socket`] on a socket
    /// failure.
    pub async fn next(&mut self) -> Result<Incoming, ClientError> {
        self.next_within(DEFAULT_TIMEOUT).await
    }

    /// Read the next event with an explicit deadline.
    ///
    /// # Errors
    ///
    /// [`ClientError::Silent`] when nothing arrives in time, [`ClientError::Socket`] on a socket
    /// failure.
    pub async fn next_within(&mut self, deadline: Duration) -> Result<Incoming, ClientError> {
        loop {
            let frame = tokio::time::timeout(deadline, self.socket.next())
                .await
                .map_err(|_| ClientError::Silent(deadline))?;
            match frame {
                Some(Ok(Message::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let kind = value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    return Ok(Incoming::Event { kind, value });
                }
                Some(Ok(Message::Close(frame))) => {
                    return Ok(match frame {
                        Some(frame) => Incoming::Closed {
                            code: u16::from(frame.code),
                            reason: frame.reason.to_string(),
                        },
                        None => Incoming::Closed {
                            code: 1005,
                            reason: String::new(),
                        },
                    })
                }
                // A transport failure with no close frame is what `socket_drop` produces, and a
                // browser reports it as 1006 rather than as an error the page can read.
                Some(Err(_)) | None => return Ok(Incoming::Dropped),
                Some(Ok(_)) => {}
            }
        }
    }

    /// Read until an event of `kind` arrives, collecting everything seen on the way.
    ///
    /// Pings are answered while waiting, because a caller that is waiting for an answer is
    /// behaving like the page, and the page answers pings.
    ///
    /// # Errors
    ///
    /// [`ClientError::Silent`] when the wanted event does not arrive before the deadline, and
    /// [`ClientError::Socket`] on a socket failure. A close or a drop ends the wait and is
    /// returned as the last collected item, so the caller can see how the conversation died.
    pub async fn collect_until(
        &mut self,
        kind: &str,
        deadline: Duration,
    ) -> Result<Vec<Incoming>, ClientError> {
        let start = std::time::Instant::now();
        let mut seen = Vec::new();
        loop {
            let left = deadline
                .checked_sub(start.elapsed())
                .ok_or(ClientError::Silent(deadline))?;
            let event = self.next_within(left).await?;
            let finished = match &event {
                Incoming::Event { kind: got, value } => {
                    if got == "ping" {
                        let ping = value.clone();
                        self.pong(&ping).await?;
                    }
                    got == kind
                }
                Incoming::Closed { .. } | Incoming::Dropped => true,
            };
            seen.push(event);
            if finished {
                return Ok(seen);
            }
        }
    }

    /// Close the conversation politely.
    ///
    /// # Errors
    ///
    /// [`ClientError::Socket`] when the close frame cannot be written.
    pub async fn close(mut self) -> Result<(), ClientError> {
        self.socket
            .close(None)
            .await
            .map_err(|e| ClientError::Socket(e.to_string()))
    }
}
