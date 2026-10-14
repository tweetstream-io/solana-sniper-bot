//! Local stand-ins for a Solana RPC (HTTP JSON-RPC) and its PubSub WebSocket, so sending and
//! confirming run over real sockets without a chain.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

/// Sees every JSON-RPC request body as it arrives.
pub type OnRequest = Arc<dyn Fn(&Value) + Send + Sync>;

/// Decides what a subscribed signature reports: `Some(err)` (`null` for success) after 100 ms, or
/// `None` to stay silent.
pub type Answer = Arc<dyn Fn(&str) -> Option<Value> + Send + Sync>;

async fn serve_http(mut socket: TcpStream, status: u16, on_request: OnRequest) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let Some(header_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).to_lowercase();
        let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse().unwrap());
        while buffer.len() < header_end + 4 + length {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        }
        let body: Value = serde_json::from_slice(&buffer[header_end + 4..header_end + 4 + length]).unwrap();
        buffer.drain(..header_end + 4 + length);
        on_request(&body);
        let reply = json!({ "jsonrpc": "2.0", "id": body["id"], "result": "accepted" }).to_string();
        let response = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{reply}", reply.len());
        if socket.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Starts an HTTP JSON-RPC stand-in that answers every request with `status` and returns its URL.
pub async fn rpc_server(status: u16, on_request: OnRequest) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve_http(socket, status, Arc::clone(&on_request)));
        }
    });
    url
}

/// Starts a PubSub stand-in that answers `signatureSubscribe` and returns its URL.
pub async fn pubsub(answer: Answer) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let answer = Arc::clone(&answer);
            tokio::spawn(async move {
                let Ok(ws) = tokio_tungstenite::accept_async(socket).await else { return };
                let (sink, mut frames) = ws.split();
                let sink = Arc::new(tokio::sync::Mutex::new(sink));
                let mut next = 0u64;
                while let Some(Ok(Message::Text(text))) = frames.next().await {
                    let request: Value = serde_json::from_str(text.as_str()).unwrap();
                    if request["method"] != "signatureSubscribe" {
                        continue;
                    }
                    next += 1;
                    let subscription = next;
                    let reply = json!({ "jsonrpc": "2.0", "id": request["id"], "result": subscription }).to_string();
                    if sink.lock().await.send(Message::text(reply)).await.is_err() {
                        return;
                    }
                    let Some(err) = answer(request["params"][0].as_str().unwrap_or("")) else { continue };
                    let sink = Arc::clone(&sink);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let params =
                            json!({ "subscription": subscription, "result": { "context": { "slot": 1 }, "value": { "err": err } } });
                        let note = json!({ "jsonrpc": "2.0", "method": "signatureNotification", "params": params }).to_string();
                        let _ = sink.lock().await.send(Message::text(note)).await;
                    });
                }
            });
        }
    });
    url
}
