//! extract oversized embedded base64 attachments from transcript event
//! payloads onto the HTTP blob store before the event rides the WS.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use base64::Engine;
use cctui_proto::adapter::AdapterEvent;
use cctui_proto::blob::{BLOB_THRESHOLD_BYTES, BlobRef};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::client::ServerClient;

/// Hashes already confirmed present on the server, so a repeated screenshot is
/// not re-uploaded. The server PUT is idempotent regardless; this is a cheap
/// short-circuit shared across every session in the process.
static KNOWN_PRESENT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

struct Candidate {
    hash: String,
    size: u64,
    media_type: Option<String>,
    bytes: Vec<u8>,
}

fn candidate(map: &Map<String, Value>) -> Option<Candidate> {
    if map.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let data = map.get("data").and_then(Value::as_str)?;
    // Decoded length is ~3/4 the base64 length; skip the decode when it can't
    // possibly clear the threshold.
    if data.len() / 4 * 3 <= BLOB_THRESHOLD_BYTES {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    if bytes.len() <= BLOB_THRESHOLD_BYTES {
        return None;
    }
    let media_type = map.get("media_type").and_then(Value::as_str).map(str::to_owned);
    let hash = hex::encode(Sha256::digest(&bytes));
    let size = bytes.len() as u64;
    Some(Candidate { hash, size, media_type, bytes })
}

fn collect(value: &Value, out: &mut Vec<Candidate>) {
    match value {
        Value::Object(map) => {
            if let Some(c) = candidate(map) {
                out.push(c);
                return;
            }
            for v in map.values() {
                collect(v, out);
            }
        }
        Value::Array(arr) => arr.iter().for_each(|v| collect(v, out)),
        _ => {}
    }
}

fn replace(value: &mut Value, uploaded: &HashSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(c) = candidate(map) {
                if uploaded.contains(&c.hash) {
                    *map = serde_json::to_value(BlobRef::new(c.hash, c.size, c.media_type))
                        .ok()
                        .and_then(|v| v.as_object().cloned())
                        .unwrap_or_else(|| map.clone());
                }
                return;
            }
            for v in map.values_mut() {
                replace(v, uploaded);
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(|v| replace(v, uploaded)),
        _ => {}
    }
}

async fn ensure_uploaded(client: &ServerClient, machine_key: &str, c: &Candidate) -> bool {
    if KNOWN_PRESENT.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&c.hash) {
        return true;
    }
    match client.put_blob(machine_key, &c.hash, c.bytes.clone(), c.media_type.as_deref()).await {
        Ok(()) => {
            KNOWN_PRESENT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(c.hash.clone());
            true
        }
        Err(err) => {
            tracing::warn!(hash = %c.hash, %err, "blob upload failed; leaving base64 inline");
            false
        }
    }
}

/// Upload oversized base64 attachments and rewrite them to a [`BlobRef`].
///
/// On any upload failure that block is left inline (data is never lost). Only
/// `Message`/`ToolUse` payloads are scanned; other events pass through.
pub async fn extract_blobs(
    client: &ServerClient,
    machine_key: &str,
    event: AdapterEvent,
) -> AdapterEvent {
    let (local_id, payload, turn_id, is_tool) = match event {
        AdapterEvent::Message { local_id, payload, turn_id } => (local_id, payload, turn_id, false),
        AdapterEvent::ToolUse { local_id, payload } => (local_id, payload, None, true),
        other => return other,
    };

    let mut candidates = Vec::new();
    collect(&payload, &mut candidates);
    if candidates.is_empty() {
        return rebuild(local_id, payload, turn_id, is_tool);
    }

    let mut uploaded = HashSet::new();
    let mut seen = HashSet::new();
    for c in &candidates {
        if !seen.insert(c.hash.clone()) {
            continue;
        }
        if ensure_uploaded(client, machine_key, c).await {
            uploaded.insert(c.hash.clone());
        }
    }
    if uploaded.is_empty() {
        return rebuild(local_id, payload, turn_id, is_tool);
    }

    let mut payload = payload;
    replace(&mut payload, &uploaded);
    rebuild(local_id, payload, turn_id, is_tool)
}

const fn rebuild(
    local_id: String,
    payload: Value,
    turn_id: Option<uuid::Uuid>,
    is_tool: bool,
) -> AdapterEvent {
    if is_tool {
        AdapterEvent::ToolUse { local_id, payload }
    } else {
        AdapterEvent::Message { local_id, payload, turn_id }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cctui_proto::blob::BLOB_SOURCE_TYPE;
    use serde_json::json;

    fn big_b64(byte: u8, len: usize) -> String {
        base64::engine::general_purpose::STANDARD.encode(vec![byte; len])
    }

    fn image_payload(data: &str) -> Value {
        json!({
            "kind": "tool_result",
            "content": [{
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": data }
            }],
        })
    }

    #[test]
    fn collect_finds_only_oversized_base64() {
        let big = big_b64(1, BLOB_THRESHOLD_BYTES + 10);
        let small = big_b64(2, 100);
        let payload = json!({
            "content": [
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": big } },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": small } },
            ],
        });
        let mut out = Vec::new();
        collect(&payload, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].size as usize > BLOB_THRESHOLD_BYTES);
        assert_eq!(out[0].media_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn replace_rewrites_matched_hashes_to_blob_ref() {
        let big = big_b64(3, BLOB_THRESHOLD_BYTES + 10);
        let mut payload = image_payload(&big);
        let mut cands = Vec::new();
        collect(&payload, &mut cands);
        let uploaded: HashSet<String> = cands.iter().map(|c| c.hash.clone()).collect();
        replace(&mut payload, &uploaded);
        let src = &payload["content"][0]["source"];
        assert_eq!(src["type"], BLOB_SOURCE_TYPE);
        assert_eq!(src["blob_id"], cands[0].hash);
        assert!(src.get("data").is_none());
    }

    #[test]
    fn replace_leaves_unuploaded_inline() {
        let big = big_b64(4, BLOB_THRESHOLD_BYTES + 10);
        let mut payload = image_payload(&big);
        replace(&mut payload, &HashSet::new());
        assert_eq!(payload["content"][0]["source"]["type"], "base64");
        assert_eq!(payload["content"][0]["source"]["data"], big);
    }

    /// Accept one PUT and reply 200, capturing the request line.
    async fn serve_put(
        status: &'static str,
    ) -> (String, std::sync::Arc<tokio::sync::Mutex<String>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let sink = captured.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            *sink.lock().await = String::from_utf8_lossy(&buf[..n]).into_owned();
            sock.write_all(status.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        (format!("http://{addr}"), captured)
    }

    #[tokio::test]
    async fn oversized_blob_extracted_and_replaced_on_success() {
        let (url, req) = serve_put("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        let client = ServerClient::new(url);
        let big = big_b64(5, BLOB_THRESHOLD_BYTES + 10);
        let event = AdapterEvent::ToolUse { local_id: "s1".into(), payload: image_payload(&big) };
        let out = extract_blobs(&client, "mkey", event).await;
        let AdapterEvent::ToolUse { payload, .. } = out else { panic!("wrong variant") };
        assert_eq!(payload["content"][0]["source"]["type"], BLOB_SOURCE_TYPE);
        assert!(req.lock().await.starts_with("PUT /api/v1/daemon/blobs/"));
    }

    #[tokio::test]
    async fn upload_failure_falls_back_to_inline() {
        let (url, _req) =
            serve_put("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n").await;
        let client = ServerClient::new(url);
        let big = big_b64(6, BLOB_THRESHOLD_BYTES + 10);
        let event = AdapterEvent::ToolUse { local_id: "s1".into(), payload: image_payload(&big) };
        let out = extract_blobs(&client, "mkey", event).await;
        let AdapterEvent::ToolUse { payload, .. } = out else { panic!("wrong variant") };
        assert_eq!(payload["content"][0]["source"]["type"], "base64");
        assert_eq!(payload["content"][0]["source"]["data"], big);
    }

    #[tokio::test]
    async fn small_blob_untouched_no_upload() {
        // Port that refuses connections: any upload attempt would error. The
        // small blob must never reach it.
        let client = ServerClient::new("http://127.0.0.1:1");
        let small = big_b64(7, 200);
        let event = AdapterEvent::ToolUse { local_id: "s1".into(), payload: image_payload(&small) };
        let out = extract_blobs(&client, "mkey", event).await;
        let AdapterEvent::ToolUse { payload, .. } = out else { panic!("wrong variant") };
        assert_eq!(payload["content"][0]["source"]["data"], small);
    }
}
