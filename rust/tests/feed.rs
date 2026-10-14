//! The feed client against a local stand-in for the TweetStream WebSocket: subprotocol auth, a
//! rejected key, and the first envelope.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use solana_sniper_bot::feed::client::{FeedOptions, spawn_feed};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};

const GOOD_KEY: &str = "good-key";

/// Selects `tweetstream.v1` for a request that offers the good key, answers 401 otherwise, and
/// replies to `auth_ping` with `control/auth_pong`.
async fn feed_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                #[expect(clippy::result_large_err, reason = "tungstenite fixes the handshake callback's signature")]
                let check = |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
                    let offered = request.headers().get("sec-websocket-protocol").and_then(|v| v.to_str().ok()).unwrap_or("");
                    let protocols: Vec<&str> = offered.split(',').map(str::trim).collect();
                    if !protocols.contains(&"tweetstream.v1") || !protocols.contains(&format!("tweetstream.auth.token.{GOOD_KEY}").as_str())
                    {
                        let mut rejected = ErrorResponse::new(Some("unknown API key".to_string()));
                        *rejected.status_mut() = StatusCode::UNAUTHORIZED;
                        return Err(rejected);
                    }
                    response.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static("tweetstream.v1"));
                    Ok(response)
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(socket, check).await else {
                    return;
                };
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    if text.as_str().contains("auth_ping") {
                        let pong = r#"{"v":1,"t":"control","op":"auth_pong","ts":1,"d":{}}"#;
                        if ws.send(Message::text(pong)).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    url
}

fn options(url: &str, key: &str) -> FeedOptions {
    FeedOptions { name: "standard".to_string(), api_key: key.to_string(), url: url.to_string(), heartbeat: Duration::from_secs(30) }
}

#[tokio::test]
async fn a_rejected_key_is_fatal_and_names_the_dashboard() {
    let url = feed_server().await;
    let (fatal, mut reasons) = mpsc::unbounded_channel();
    let task = spawn_feed(options(&url, "wrong-key"), Arc::new(|_, _| {}), fatal);
    let reason = tokio::time::timeout(Duration::from_secs(5), reasons.recv()).await.unwrap().unwrap();
    task.abort();
    assert!(reason.contains("rejected (401)"), "{reason}");
    assert!(reason.contains("tweetstream.io/dashboard"), "{reason}");
}

#[tokio::test]
async fn a_valid_key_authenticates_and_receives_auth_pong() {
    let url = feed_server().await;
    let (fatal, _reasons) = mpsc::unbounded_channel();
    let (events, mut received) = mpsc::unbounded_channel();
    let handler = Arc::new(move |envelope: &solana_sniper_bot::protocol::Envelope, receipt: &solana_sniper_bot::feed::client::Receipt| {
        let _ = events.send(format!("{}/{} on {}", envelope.t, envelope.op, receipt.connection));
    });
    let task = spawn_feed(options(&url, GOOD_KEY), handler, fatal);
    let first = tokio::time::timeout(Duration::from_secs(5), received.recv()).await.unwrap().unwrap();
    task.abort();
    assert_eq!(first, "control/auth_pong on standard");
}
