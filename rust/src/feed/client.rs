//! One TweetStream WebSocket connection: authenticate, keep alive, reconnect with jittered backoff,
//! and stamp each frame on receipt. Run one per API key (standard, and Ultra Speed if you have it).
//! Next: `feed/dedupe.rs` drops the second copy, then `pipeline.rs` routes the event.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::log::{log, unix_ms};
use crate::protocol::Envelope;
use crate::setup::env::product_link;

/// When and where a frame arrived.
#[derive(Debug, Clone)]
pub struct Receipt {
    /// `standard` or `ultra`.
    pub connection: String,
    /// Wall clock in Unix ms, comparable with post publish times.
    pub wall_ms: i64,
    /// Monotonic, for timing the bot's own stages.
    pub mono: Instant,
}

/// One feed connection's settings.
#[derive(Debug, Clone)]
pub struct FeedOptions {
    /// `standard` or `ultra`.
    pub name: String,
    /// The API key, sent as a subprotocol so it never appears in a URL or a proxy log.
    pub api_key: String,
    /// WebSocket URL.
    pub url: String,
    /// `auth_ping` interval, which doubles as a liveness check.
    pub heartbeat: Duration,
    /// Another connection carries the same events, so a refused key (401 or 403) only closes this
    /// one. Set for the Ultra Speed connection when a standard key runs too.
    pub optional: bool,
}

/// Called for every v1 envelope, on the task that read it.
pub type EventHandler = Arc<dyn Fn(&Envelope, &Receipt) + Send + Sync>;

const AUTH_PING: &str = r#"{"op":"auth_ping"}"#;

/// Returns the reconnect delay: exponential from 0.5 s, capped at 30 s, with jitter in the upper
/// half, so many clients do not reconnect in lockstep after an outage. `random` is in `[0, 1)`.
///
/// # Examples
///
/// ```
/// use solana_sniper_bot::feed::client::backoff_ms;
/// assert_eq!(backoff_ms(0, 0.0), 250);
/// assert_eq!(backoff_ms(10, 0.999), 29_985);
/// ```
pub fn backoff_ms(attempt: u32, random: f64) -> u64 {
    let base = 30_000f64.min(500.0 * 2f64.powi(attempt.min(16) as i32));
    (base / 2.0 + random * (base / 2.0)).floor() as u64
}

/// Starts one connection that reconnects until aborted. A rejected key goes to `fatal`, except on
/// an optional connection, where it is logged and ends only that connection.
pub fn spawn_feed(options: FeedOptions, on_event: EventHandler, fatal: mpsc::UnboundedSender<String>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut attempt = 0u32;
        loop {
            let reason = match connect(&options).await {
                Ok(stream) => {
                    let opened = Instant::now();
                    log("connected", &format!("{} {}", options.name, options.url));
                    let reason = session(stream, &options, &on_event).await;
                    if opened.elapsed() >= Duration::from_secs(30) {
                        attempt = 0;
                    }
                    reason
                }
                Err(WsError::Http(response)) => {
                    let status = response.status().as_u16();
                    log("refused", &format!("{}: handshake answered HTTP {status}", options.name));
                    match status {
                        401 | 403 if options.optional => {
                            let detail = format!("was refused ({status}); continuing on the other connection");
                            log("refused", &format!("{} key {detail}", options.name));
                            return;
                        }
                        401 => {
                            let dashboard = product_link("/dashboard?tab=api", "rust-cli-key-rejected");
                            let _ = fatal.send(format!("{} API key rejected (401). Copy a current key from {dashboard}", options.name));
                            return;
                        }
                        403 => {
                            let _ = fatal.send(format!("{} key cannot stream (403). Start a trial or renew your plan.", options.name));
                            return;
                        }
                        429 => log(
                            "limit",
                            &format!("{}: every connection allowed for this key is in use; close another client", options.name),
                        ),
                        _ => {}
                    }
                    format!("HTTP {status}")
                }
                Err(error) => error.to_string(),
            };
            let delay = backoff_ms(attempt, rand::random::<f64>());
            attempt = (attempt + 1).min(6);
            log("closed", &format!("{}: {reason}; reconnecting in {:.1} s", options.name, delay as f64 / 1000.0));
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    })
}

type Stream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens the socket with TCP_NODELAY and no compression: inflating small JSON frames costs CPU on
/// the hot path, and Nagle's algorithm would hold the small `auth_ping` frames.
async fn connect(options: &FeedOptions) -> Result<Stream, WsError> {
    let mut request = options.url.as_str().into_client_request()?;
    let protocols = format!("tweetstream.v1, tweetstream.auth.token.{}", options.api_key);
    let header = HeaderValue::from_str(&protocols).map_err(|e| WsError::HttpFormat(e.into()))?;
    request.headers_mut().insert("Sec-WebSocket-Protocol", header);
    let (stream, _response) = tokio_tungstenite::connect_async_with_config(request, None, true).await?;
    Ok(stream)
}

/// Reads frames until the connection ends and returns why. A half-open TCP connection after a
/// network change goes silent, so a connection with no frame for two heartbeats is dropped.
async fn session(stream: Stream, options: &FeedOptions, on_event: &EventHandler) -> String {
    let (mut sink, mut frames) = stream.split();
    if let Err(error) = sink.send(Message::text(AUTH_PING)).await {
        return error.to_string();
    }
    let stale_after = options.heartbeat * 2 + Duration::from_secs(5);
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + options.heartbeat, options.heartbeat);
    let mut last_frame = Instant::now();
    loop {
        tokio::select! {
            frame = frames.next() => {
                let mono = Instant::now();
                last_frame = mono;
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(data))) => {
                        if let Err(error) = sink.send(Message::Pong(data)).await {
                            return error.to_string();
                        }
                        continue;
                    }
                    Some(Ok(Message::Close(frame))) => return frame.map_or("closed".to_string(), |f| format!("code {}", u16::from(f.code))),
                    Some(Ok(Message::Binary(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                    Some(Err(error)) => return error.to_string(),
                    None => return "closed".to_string(),
                };
                let receipt = Receipt { connection: options.name.clone(), wall_ms: unix_ms(), mono };
                match serde_json::from_str::<Envelope>(text.as_str()) {
                    Ok(envelope) if envelope.v == Some(1) => on_event(&envelope, &receipt),
                    Ok(_) => {}
                    Err(_) => log("skip", &format!("{}: frame is not a v1 envelope", options.name)),
                }
            }
            _ = heartbeat.tick() => {
                if last_frame.elapsed() > stale_after {
                    log("stale", &format!("{}: no frames for {} s", options.name, stale_after.as_secs()));
                    return "stale".to_string();
                }
                if let Err(error) = sink.send(Message::text(AUTH_PING)).await {
                    return error.to_string();
                }
            }
        }
    }
}
