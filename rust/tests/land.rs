//! Landing against local stand-ins for an RPC (HTTP `sendTransaction`) and its PubSub WebSocket
//! (`signatureSubscribe`), so fan-out and confirmation run over real sockets without a chain.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_sniper_bot::execute::trade::{Signed, compile_and_sign};
use solana_sniper_bot::land::land::{Lander, LanderOptions, SenderKind, SenderTarget, SignatureWatcher, Status};
use solana_sniper_bot::network::http_client;

fn signed_transfer() -> Signed {
    let payer = Keypair::new();
    let transfer = solana_system_interface::instruction::transfer(&payer.pubkey(), &Keypair::new().pubkey(), 1);
    compile_and_sign(&payer, &[transfer], Hash::new_from_array([9; 32])).unwrap()
}

type Bodies = Arc<Mutex<Vec<Value>>>;

async fn rpc_server(status: u16) -> (String, Bodies) {
    let bodies = Bodies::default();
    let seen = Arc::clone(&bodies);
    let url = common::rpc_server(status, Arc::new(move |body: &Value| seen.lock().unwrap().push(body.clone()))).await;
    (url, bodies)
}

async fn pubsub(err: Value) -> String {
    common::pubsub(Arc::new(move |_signature: &str| Some(err.clone()))).await
}

async fn land_once(err: Value) -> (Signed, solana_sniper_bot::land::land::Landing, Bodies, Bodies) {
    let (down, down_bodies) = rpc_server(500).await;
    let (up, up_bodies) = rpc_server(200).await;
    let (watcher, _task) = SignatureWatcher::start(pubsub(err).await);
    let targets = vec![
        SenderTarget { name: "down".to_string(), url: down, kind: SenderKind::Rpc },
        SenderTarget { name: "up".to_string(), url: up, kind: SenderKind::Rpc },
    ];
    let options = LanderOptions {
        rebroadcast: Duration::from_secs(10),
        confirm_timeout: Duration::from_secs(2),
        current_block_height: Arc::new(|| Some(0)),
        status_rpc_url: None,
    };
    let lander = Lander::new(http_client(), watcher, options);
    let signed = signed_transfer();
    let landing = lander.land(&signed.bytes, &signed.signature().unwrap(), Some(1_000), targets).await;
    (signed, landing, down_bodies, up_bodies)
}

#[tokio::test]
async fn the_same_bytes_go_to_every_sender_the_first_to_accept_is_recorded_and_confirmation_is_pushed() {
    let (signed, landing, down, up) = land_once(Value::Null).await;
    assert_eq!(landing.status, Status::Confirmed);
    assert_eq!(landing.first_ack_by.as_deref(), Some("up"));
    assert_eq!(Some(landing.signature), signed.signature());
    let wire = base64::engine::general_purpose::STANDARD.encode(&signed.bytes);
    for bodies in [down, up] {
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["method"], "sendTransaction");
        assert_eq!(bodies[0]["params"][0], wire.as_str());
        assert_eq!(bodies[0]["params"][1], json!({ "encoding": "base64", "skipPreflight": true, "maxRetries": 0 }));
    }
    assert!(landing.confirmed.unwrap() > landing.sent);
}

#[tokio::test]
async fn a_transaction_that_lands_with_an_error_is_reported_as_failed() {
    let (_signed, landing, _down, _up) = land_once(json!({ "InstructionError": [2, { "Custom": 6002 }] })).await;
    match landing.status {
        Status::Failed(error) => assert!(error.contains("6002"), "{error}"),
        other => panic!("expected a failure, got {other:?}"),
    }
}
