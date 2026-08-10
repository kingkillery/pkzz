use super::*;
use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    queue::{BatchEvent, FlushBatch},
    relay::RestClient,
};
use nostr::{EventBuilder, EventId, Keys, Kind, Tag};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

fn temp_outbox_dir() -> PathBuf {
    std::env::temp_dir().join(format!("buzz-acp-host-final-outbox-{}", Uuid::new_v4()))
}

fn signed_pending_fixture(
    final_content: &str,
) -> (Keys, FinalReplyTarget, EventId, EventId, Vec<u8>, PathBuf) {
    let keys = Keys::generate();
    let channel_id = Uuid::new_v4();
    let source_event_id = EventId::from_hex(&"a".repeat(64)).expect("valid test source id");
    let trigger = EventBuilder::new(Kind::Custom(9), "trigger")
        .tags([Tag::parse(["h", &channel_id.to_string()]).expect("channel tag")])
        .sign_with_keys(&keys)
        .expect("signed trigger");
    let target = FinalReplyTarget::from_batch(&FlushBatch {
        channel_id,
        events: vec![BatchEvent {
            event: trigger,
            prompt_tag: "test".to_string(),
            received_at: std::time::Instant::now(),
        }],
        cancelled_events: Vec::new(),
        cancel_reason: None,
    })
    .expect("valid trigger target");
    let event = EventBuilder::new(Kind::Custom(9), final_content)
        .tags([Tag::parse(["h", &channel_id.to_string()]).expect("channel tag")])
        .sign_with_keys(&keys)
        .expect("signed final event");
    let event_id = event.id;
    let event_bytes = serde_json::to_vec(&event).expect("event JSON");

    (
        keys,
        target,
        source_event_id,
        event_id,
        event_bytes,
        temp_outbox_dir(),
    )
}

async fn read_http_request_body(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    let mut scratch = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut scratch)
            .await
            .expect("read test HTTP request");
        assert!(read > 0, "client closed test HTTP request early");
        received.extend_from_slice(&scratch[..read]);
        let Some(header_end) = received
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        else {
            continue;
        };
        let headers =
            std::str::from_utf8(&received[..header_end]).expect("test request headers UTF-8");
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
            })
            .expect("test request content length");
        if received.len() >= header_end + content_length {
            return received[header_end..header_end + content_length].to_vec();
        }
    }
}

async fn receipt_server(
    event_id: String,
    accepted_receipts: Vec<bool>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test HTTP listener");
    let address = listener.local_addr().expect("read test listener address");
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for accepted in accepted_receipts {
            let (mut stream, _) = listener.accept().await.expect("accept test HTTP client");
            bodies.push(read_http_request_body(&mut stream).await);
            let receipt = format!("{{\"event_id\":\"{event_id}\",\"accepted\":{accepted}}}");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{receipt}",
                receipt.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write test HTTP response");
        }
        bodies
    });
    (address, server)
}

fn assert_compacted_tombstone(directory: &Path, event_id: &EventId, final_content: &str) {
    let persisted = fs::read_to_string(directory.join(format!("{}.json", event_id.to_hex())))
        .expect("read persisted record");
    assert!(
        !persisted.contains(final_content),
        "terminal tombstone retained final-message content"
    );
    assert!(
        !persisted.contains("event_bytes_b64"),
        "terminal tombstone retained the encoded event body"
    );
}

#[test]
fn pending_signed_record_survives_reopen_with_exact_bytes_and_fences_sources() {
    let (keys, target, source_event_id, event_id, event_bytes, directory) =
        signed_pending_fixture("pending-final-message");
    let relay_url = "ws://127.0.0.1:3000";

    let outbox = HostFinalDeliveryOutbox::open(&directory, relay_url, &keys.public_key())
        .expect("outbox opens");
    outbox
        .persist_signed(
            event_id,
            event_bytes.clone(),
            &target,
            &[source_event_id, target.trigger_event_id],
        )
        .expect("record persists");
    assert!(outbox.contains_source_event(&source_event_id));
    assert!(outbox.contains_source_event(&target.trigger_event_id));

    let reopened = HostFinalDeliveryOutbox::open(&directory, relay_url, &keys.public_key())
        .expect("outbox reopens");
    assert!(reopened.contains_source_event(&source_event_id));
    assert!(reopened.contains_source_event(&target.trigger_event_id));
    let stored = reopened.record(&event_id.to_hex()).expect("record indexed");
    assert!(matches!(stored.state, HostFinalDeliveryState::Pending));
    assert_eq!(
        decode_event_bytes(&stored).expect("stored bytes"),
        event_bytes,
        "pending recovery must use the one originally signed serialization"
    );

    fs::remove_dir_all(directory).expect("cleanup temp outbox");
}

#[tokio::test]
async fn accepted_record_compacts_body_and_fences_sources_after_reopen() {
    let final_content = "accepted-final-message-must-not-persist";
    let (keys, target, source_event_id, event_id, event_bytes, directory) =
        signed_pending_fixture(final_content);
    let relay_url = "ws://127.0.0.1:3000";
    let (address, server) = receipt_server(event_id.to_hex(), vec![false, true]).await;
    let outbox = HostFinalDeliveryOutbox::open(&directory, relay_url, &keys.public_key())
        .expect("outbox opens");
    outbox
        .persist_signed(
            event_id,
            event_bytes.clone(),
            &target,
            &[source_event_id, target.trigger_event_id],
        )
        .expect("record persists");
    let client = RestClient {
        http: reqwest::Client::new(),
        base_url: format!("http://{address}"),
        keys,
        auth_tag: None,
        auth_tag_json: None,
    };

    let outcome = outbox
        .deliver_until_terminal(&event_id, &client, false)
        .await;
    assert_eq!(outcome.disposition, HostFinalDeliveryDisposition::Accepted);
    let bodies = server.await.expect("join test HTTP server");
    assert_eq!(bodies, vec![event_bytes.clone(), event_bytes]);
    assert!(matches!(
        outbox
            .record(&event_id.to_hex())
            .expect("persisted record")
            .state,
        HostFinalDeliveryState::Accepted
    ));
    assert!(
        outbox
            .record(&event_id.to_hex())
            .expect("persisted record")
            .event_bytes_b64
            .is_none(),
        "accepted in-memory tombstone retained the body"
    );
    assert_compacted_tombstone(&directory, &event_id, final_content);

    let reopened = HostFinalDeliveryOutbox::open(&directory, relay_url, &client.keys.public_key())
        .expect("outbox reopens");
    assert!(reopened.contains_source_event(&source_event_id));
    assert!(reopened.contains_source_event(&target.trigger_event_id));
    let stored = reopened.record(&event_id.to_hex()).expect("record indexed");
    assert!(matches!(stored.state, HostFinalDeliveryState::Accepted));
    assert!(stored.event_bytes_b64.is_none());

    fs::remove_dir_all(directory).expect("cleanup temp outbox");
}

#[tokio::test]
async fn terminal_record_compacts_body_and_fences_sources_after_reopen() {
    let final_content = "terminal-final-message-must-not-persist";
    let (keys, target, source_event_id, event_id, event_bytes, directory) =
        signed_pending_fixture(final_content);
    let relay_url = "ws://127.0.0.1:3000";
    let (address, server) = receipt_server(event_id.to_hex(), vec![false, false]).await;
    let outbox = HostFinalDeliveryOutbox::open(&directory, relay_url, &keys.public_key())
        .expect("outbox opens");
    outbox
        .persist_signed(
            event_id,
            event_bytes.clone(),
            &target,
            &[source_event_id, target.trigger_event_id],
        )
        .expect("record persists");
    let client = RestClient {
        http: reqwest::Client::new(),
        base_url: format!("http://{address}"),
        keys,
        auth_tag: None,
        auth_tag_json: None,
    };

    let outcome = outbox
        .deliver_until_terminal(&event_id, &client, false)
        .await;
    assert_eq!(
        outcome.disposition,
        HostFinalDeliveryDisposition::TerminalFailed
    );
    let bodies = server.await.expect("join test HTTP server");
    assert_eq!(bodies, vec![event_bytes.clone(), event_bytes]);
    assert!(matches!(
        outbox
            .record(&event_id.to_hex())
            .expect("persisted record")
            .state,
        HostFinalDeliveryState::TerminalFailed
    ));
    assert!(
        outbox
            .record(&event_id.to_hex())
            .expect("persisted record")
            .event_bytes_b64
            .is_none(),
        "terminal in-memory tombstone retained the body"
    );
    assert_compacted_tombstone(&directory, &event_id, final_content);

    let reopened = HostFinalDeliveryOutbox::open(&directory, relay_url, &client.keys.public_key())
        .expect("outbox reopens");
    assert!(reopened.contains_source_event(&source_event_id));
    assert!(reopened.contains_source_event(&target.trigger_event_id));
    let stored = reopened.record(&event_id.to_hex()).expect("record indexed");
    assert!(matches!(
        stored.state,
        HostFinalDeliveryState::TerminalFailed
    ));
    assert!(stored.event_bytes_b64.is_none());

    fs::remove_dir_all(directory).expect("cleanup temp outbox");
}

#[test]
fn reopen_compacts_legacy_terminal_body_and_retains_replay_fence() {
    let final_content = "legacy-terminal-message-must-not-persist";
    let (keys, target, source_event_id, event_id, event_bytes, directory) =
        signed_pending_fixture(final_content);
    let relay_url = "ws://127.0.0.1:3000";
    let outbox = HostFinalDeliveryOutbox::open(&directory, relay_url, &keys.public_key())
        .expect("outbox opens");
    outbox
        .persist_signed(
            event_id,
            event_bytes,
            &target,
            &[source_event_id, target.trigger_event_id],
        )
        .expect("record persists");

    let record_path = directory.join(format!("{}.json", event_id.to_hex()));
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).expect("read legacy record"))
            .expect("parse legacy record");
    legacy["state"] = serde_json::Value::String("accepted".to_string());
    fs::write(
        &record_path,
        serde_json::to_vec(&legacy).expect("serialize legacy record"),
    )
    .expect("seed legacy terminal record");
    drop(outbox);

    let reopened = HostFinalDeliveryOutbox::open(&directory, relay_url, &keys.public_key())
        .expect("outbox reopens and migrates legacy terminal record");
    assert!(reopened.contains_source_event(&source_event_id));
    assert!(reopened.contains_source_event(&target.trigger_event_id));
    let stored = reopened.record(&event_id.to_hex()).expect("record indexed");
    assert!(matches!(stored.state, HostFinalDeliveryState::Accepted));
    assert!(stored.event_bytes_b64.is_none());
    assert_compacted_tombstone(&directory, &event_id, final_content);

    fs::remove_dir_all(directory).expect("cleanup temp outbox");
}

#[test]
fn foreign_identity_pending_record_in_shared_dir_is_left_untouched() {
    let (keys_a, target_a, source_event_id_a, event_id_a, event_bytes_a, directory) =
        signed_pending_fixture("identity-a-pending-final");
    let keys_b = Keys::generate();
    let relay_a = "ws://127.0.0.1:3000";
    let relay_b = "ws://127.0.0.1:3001";

    let outbox_a = HostFinalDeliveryOutbox::open(&directory, relay_a, &keys_a.public_key())
        .expect("identity A outbox opens");
    outbox_a
        .persist_signed(
            event_id_a,
            event_bytes_a.clone(),
            &target_a,
            &[source_event_id_a, target_a.trigger_event_id],
        )
        .expect("identity A record persists");

    let path_a = directory.join(format!("{}.json", event_id_a.to_hex()));
    let original_bytes = fs::read(&path_a).expect("read identity A pending file");

    let outbox_b = HostFinalDeliveryOutbox::open(&directory, relay_b, &keys_b.public_key())
        .expect("identity B outbox opens shared dir");
    assert_eq!(
        outbox_b.contains_source_event(&source_event_id_a),
        false,
        "foreign source event should not be indexed by identity B"
    );
    assert_eq!(
        outbox_b.contains_source_event(&target_a.trigger_event_id),
        false,
        "foreign trigger event should not be indexed by identity B"
    );

    let after_foreign_open = fs::read(&path_a).expect("reread identity A pending file");
    assert_eq!(
        after_foreign_open, original_bytes,
        "foreign open must not rewrite another identity's pending record"
    );

    let reopened_a = HostFinalDeliveryOutbox::open(&directory, relay_a, &keys_a.public_key())
        .expect("identity A outbox reopens");
    assert!(
        reopened_a.contains_source_event(&source_event_id_a),
        "identity A pending record must remain present/deliverable after foreign open"
    );
    assert!(
        reopened_a.contains_source_event(&target_a.trigger_event_id),
        "identity A trigger fence must remain present after foreign open"
    );
    assert_eq!(
        fs::read(&path_a).expect("final reread"),
        original_bytes,
        "identity A pending bytes must still be unchanged"
    );
}
