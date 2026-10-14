//! Sends one signed transaction to every sender at once, rebroadcasts it until it confirms or its
//! blockhash expires, and learns the outcome from a `signatureSubscribe` WebSocket instead of
//! polling. Next: `report/latency.rs` records the stage times.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::feed::client::backoff_ms;
use crate::log::log;
use crate::network::{SignatureStatus, fetch_signature_status, rpc_call};

/// How a sender takes `sendTransaction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderKind {
    /// A JSON-RPC endpoint: `skipPreflight` (saves a simulation) and `maxRetries: 0`, because the
    /// bot rebroadcasts itself.
    Rpc,
    /// Jito's `/api/v1/transactions`, which takes only the encoding option and always skips
    /// preflight.
    Jito,
}

/// One endpoint the transaction goes to.
#[derive(Debug, Clone)]
pub struct SenderTarget {
    /// Name for logs and the report.
    pub name: String,
    /// Endpoint URL.
    pub url: String,
    /// How it takes the transaction.
    pub kind: SenderKind,
}

/// The outcome of a transaction on chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Confirmed without an error.
    Confirmed,
    /// Landed with an error; the transaction's effects did not apply.
    Failed(String),
    /// No answer in time: it may still land.
    Unknown(String),
}

/// What happened to one sent transaction.
#[derive(Debug, Clone)]
pub struct Landing {
    /// Base58 signature.
    pub signature: String,
    /// The outcome.
    pub status: Status,
    /// First send.
    pub sent: Instant,
    /// First sender accepted.
    pub first_ack: Option<Instant>,
    /// Which sender accepted first.
    pub first_ack_by: Option<String>,
    /// Outcome known.
    pub confirmed: Option<Instant>,
}

/// Returns the current block height, when known.
pub type BlockHeight = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// Lander settings.
#[derive(Clone)]
pub struct LanderOptions {
    /// Rebroadcast interval.
    pub rebroadcast: Duration,
    /// Give up waiting for confirmation after this long.
    pub confirm_timeout: Duration,
    /// Stops rebroadcasting once the height passes the transaction's validity window.
    pub current_block_height: BlockHeight,
    /// Asked once over HTTP when the subscription never answered.
    pub status_rpc_url: Option<String>,
}

/// Fans signed transactions out to the senders each transaction may use and waits for the outcome.
pub struct Lander {
    client: reqwest::Client,
    watcher: SignatureWatcher,
    options: LanderOptions,
}

const SEND_TIMEOUT: Duration = Duration::from_secs(5);

impl Lander {
    /// Creates a lander over a running watcher.
    pub fn new(client: reqwest::Client, watcher: SignatureWatcher, options: LanderOptions) -> Lander {
        Lander { client, watcher, options }
    }

    /// Sends the transaction to `targets` (picked per transaction by `execute/trade.rs`), rebroadcasts
    /// it while the block height stays at or below `last_valid_block_height` (until the wait times
    /// out when that is not known), and returns once the outcome is known or the wait times out.
    pub async fn land(&self, bytes: &[u8], signature: &str, last_valid_block_height: Option<u64>, targets: Vec<SenderTarget>) -> Landing {
        let signature = signature.to_string();
        let base64 = Arc::new(base64::engine::general_purpose::STANDARD.encode(bytes));
        let senders = Arc::new(targets);
        let outcome = self.watcher.wait(signature.clone());
        let sent = Instant::now();
        let first = tokio::spawn(fan_out(self.client.clone(), Arc::clone(&senders), Arc::clone(&base64)));
        let rebroadcast = {
            let (client, senders, base64) = (self.client.clone(), Arc::clone(&senders), Arc::clone(&base64));
            let current_block_height = Arc::clone(&self.options.current_block_height);
            let period = self.options.rebroadcast;
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                loop {
                    ticker.tick().await;
                    if let (Some(height), Some(last_valid)) = (current_block_height(), last_valid_block_height)
                        && height > last_valid
                    {
                        return;
                    }
                    drop(tokio::spawn(fan_out(client.clone(), Arc::clone(&senders), Arc::clone(&base64))));
                }
            })
        };
        let status = match tokio::time::timeout(self.options.confirm_timeout, outcome).await {
            Ok(Ok(status)) => status,
            Ok(Err(_)) | Err(_) => {
                self.watcher.forget(signature.clone());
                let fallback = Status::Unknown(format!("not confirmed within {} ms", self.options.confirm_timeout.as_millis()));
                self.poll_once(&signature, fallback).await
            }
        };
        rebroadcast.abort();
        let mut landing = Landing { signature, status, sent, first_ack: None, first_ack_by: None, confirmed: None };
        if !matches_unknown(&landing.status) {
            landing.confirmed = Some(Instant::now());
        }
        match first.await {
            Ok(Ok((by, at))) => {
                landing.first_ack = Some(at);
                landing.first_ack_by = Some(by);
            }
            Ok(Err(error)) => log("send", &format!("no sender accepted {}: {error}", &landing.signature[..8.min(landing.signature.len())])),
            Err(_) => {}
        }
        landing
    }

    /// Sends a stored transaction again while its blockhash is valid (the reconciler).
    pub async fn rebroadcast(&self, base64: String, targets: Vec<SenderTarget>) {
        if !targets.is_empty() {
            let _ = fan_out(self.client.clone(), Arc::new(targets), Arc::new(base64)).await;
        }
    }

    /// If the subscription never answered (the RPC WebSocket was down), asks once over HTTP.
    async fn poll_once(&self, signature: &str, fallback: Status) -> Status {
        let Some(rpc) = &self.options.status_rpc_url else {
            return fallback;
        };
        match fetch_signature_status(&self.client, rpc, signature, false, SEND_TIMEOUT).await {
            Ok(Some(SignatureStatus { confirmed: true, err: Some(err) })) => Status::Failed(err.to_string()),
            Ok(Some(SignatureStatus { confirmed: true, err: None })) => Status::Confirmed,
            Ok(Some(SignatureStatus { confirmed: false, err: _ }) | None) | Err(_) => fallback,
        }
    }
}

fn matches_unknown(status: &Status) -> bool {
    match status {
        Status::Unknown(_) => true,
        Status::Confirmed | Status::Failed(_) => false,
    }
}

/// Sends to every sender at once and returns the first to accept and when. The other sends keep
/// running in their own tasks; nothing is cancelled when one answers first.
async fn fan_out(client: reqwest::Client, senders: Arc<Vec<SenderTarget>>, base64: Arc<String>) -> Result<(String, Instant), String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    for sender in senders.iter().cloned() {
        let (client, base64, tx) = (client.clone(), Arc::clone(&base64), tx.clone());
        drop(tokio::spawn(async move {
            let options = match sender.kind {
                SenderKind::Jito => json!({ "encoding": "base64" }),
                SenderKind::Rpc => json!({ "encoding": "base64", "skipPreflight": true, "maxRetries": 0 }),
            };
            let result = rpc_call::<Value>(&client, &sender.url, "sendTransaction", json!([base64.as_str(), options]), SEND_TIMEOUT).await;
            let _ = tx.send((sender.name, result.map(|_| Instant::now()).map_err(|e| e.0)));
        }));
    }
    drop(tx);
    let mut errors = Vec::new();
    while let Some((name, result)) = rx.recv().await {
        match result {
            Ok(at) => return Ok((name, at)),
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    Err(errors.join("; "))
}

enum Command {
    Wait(String, oneshot::Sender<Status>),
    Forget(String),
}

/// One long-lived Solana PubSub connection. A confirmation arrives as a push the moment the RPC
/// sees it, where polling `getSignatureStatuses` would add up to one poll interval.
#[derive(Clone)]
pub struct SignatureWatcher {
    commands: mpsc::UnboundedSender<Command>,
}

impl SignatureWatcher {
    /// Starts the connection task; it reconnects with backoff and resubscribes pending signatures.
    pub fn start(url: String) -> (SignatureWatcher, JoinHandle<()>) {
        let (commands, rx) = mpsc::unbounded_channel();
        (SignatureWatcher { commands }, tokio::spawn(watch(url, rx)))
    }

    /// Subscribes to a signature before the send, so a fast confirmation cannot arrive unheard.
    pub fn wait(&self, signature: String) -> oneshot::Receiver<Status> {
        let (tx, rx) = oneshot::channel();
        let _ = self.commands.send(Command::Wait(signature, tx));
        rx
    }

    fn forget(&self, signature: String) {
        let _ = self.commands.send(Command::Forget(signature));
    }
}

#[derive(Default)]
struct Subscriptions {
    pending: HashMap<String, oneshot::Sender<Status>>,
    requests: HashMap<u64, String>,
    active: HashMap<u64, String>,
    next_id: u64,
}

impl Subscriptions {
    fn subscribe(&mut self, signature: &str) -> Message {
        self.next_id += 1;
        self.requests.insert(self.next_id, signature.to_string());
        let request = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": "signatureSubscribe", "params": [signature, { "commitment": "confirmed" }] });
        Message::text(request.to_string())
    }

    fn on_message(&mut self, text: &str) {
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            return log("confirm", "RPC WebSocket sent a frame that is not JSON");
        };
        if let (Some(id), Some(subscription)) = (message["id"].as_u64(), message["result"].as_u64()) {
            if let Some(signature) = self.requests.remove(&id) {
                self.active.insert(subscription, signature);
            }
            return;
        }
        if message["method"] != "signatureNotification" {
            return;
        }
        let Some(subscription) = message["params"]["subscription"].as_u64() else {
            return;
        };
        let Some(signature) = self.active.remove(&subscription) else {
            return;
        };
        let err = &message["params"]["result"]["value"]["err"];
        let status = if err.is_null() { Status::Confirmed } else { Status::Failed(err.to_string()) };
        if let Some(reply) = self.pending.remove(&signature) {
            let _ = reply.send(status);
        }
    }
}

async fn watch(url: String, mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut subs = Subscriptions::default();
    let mut attempt = 0u32;
    loop {
        let connected = tokio_tungstenite::connect_async_with_config(url.as_str(), None, true).await;
        let Ok((stream, _)) = connected else {
            let delay = Duration::from_millis(backoff_ms(attempt, rand::random::<f64>()));
            attempt = (attempt + 1).min(6);
            if !wait_for_commands(&mut commands, &mut subs, delay).await {
                return;
            }
            continue;
        };
        attempt = 0;
        let (mut sink, mut frames) = stream.split();
        subs.requests.clear();
        subs.active.clear();
        let pending: Vec<String> = subs.pending.keys().cloned().collect();
        for signature in pending {
            let request = subs.subscribe(&signature);
            if sink.send(request).await.is_err() {
                break;
            }
        }
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Wait(signature, reply)) => {
                        subs.pending.insert(signature.clone(), reply);
                        let request = subs.subscribe(&signature);
                        if sink.send(request).await.is_err() {
                            break;
                        }
                    }
                    Some(Command::Forget(signature)) => {
                        subs.pending.remove(&signature);
                    }
                    None => return,
                },
                frame = frames.next() => match frame {
                    Some(Ok(Message::Text(text))) => subs.on_message(text.as_str()),
                    Some(Ok(Message::Ping(data))) => {
                        if sink.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                },
            }
        }
    }
}

/// Keeps taking commands while disconnected, so waits are registered for the next connection.
/// Returns false when every handle is gone.
async fn wait_for_commands(commands: &mut mpsc::UnboundedReceiver<Command>, subs: &mut Subscriptions, delay: Duration) -> bool {
    let until = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Wait(signature, reply)) => {
                    subs.pending.insert(signature, reply);
                }
                Some(Command::Forget(signature)) => {
                    subs.pending.remove(&signature);
                }
                None => return false,
            },
            () = tokio::time::sleep_until(until) => return true,
        }
    }
}
