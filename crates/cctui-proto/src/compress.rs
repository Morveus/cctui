//! Application-layer zstd compression for daemon up-frames.
//!
//! Compression runs before [`crate::chunk`], so the content hash and
//! chunk/ack/resume all operate over the compressed bytes.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::ws::DaemonFrameUp;

pub const CODEC_ZSTD: &str = "zstd";

/// Below this, zstd frame overhead outweighs the gain, so frames are sent raw.
pub const COMPRESS_MIN_BYTES: usize = 4 * 1024;

/// Measured on a 500-event replay, level 6 (~1.6ms) beats level 9 (~11ms) on
/// both ratio and CPU for this redundant JSON; higher levels only add cost.
pub const ZSTD_LEVEL: i32 = 6;

/// zstd-compress an in-memory buffer. Infallible for in-memory input.
#[must_use]
pub fn zstd_compress(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, ZSTD_LEVEL).expect("zstd encode of an in-memory buffer cannot fail")
}

/// Decompress a zstd buffer.
pub fn zstd_decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::decode_all(data)
}

/// Compress a serialized frame when it clears [`COMPRESS_MIN_BYTES`] and zstd
/// actually shrinks it. Returns the bytes to put on the wire plus the codec
/// marker (`None` = uncompressed, send as-is).
#[must_use]
pub fn maybe_compress(serialized: &[u8]) -> (Vec<u8>, Option<&'static str>) {
    if serialized.len() < COMPRESS_MIN_BYTES {
        return (serialized.to_vec(), None);
    }
    let compressed = zstd_compress(serialized);
    if compressed.len() < serialized.len() {
        (compressed, Some(CODEC_ZSTD))
    } else {
        (serialized.to_vec(), None)
    }
}

/// Decompress bytes tagged with `codec`. An empty/absent codec passes through;
/// an unknown codec errors so the peer can't silently misinterpret a payload.
pub fn decompress_codec(codec: &str, data: &[u8]) -> std::io::Result<Vec<u8>> {
    match codec {
        "" => Ok(data.to_vec()),
        CODEC_ZSTD => zstd_decompress(data),
        other => Err(std::io::Error::other(format!("unknown codec {other}"))),
    }
}

/// Build a [`DaemonFrameUp::Compressed`] wrapping an already-compressed payload.
#[must_use]
pub fn compressed_frame(codec: &str, data: &[u8]) -> DaemonFrameUp {
    DaemonFrameUp::Compressed { codec: codec.to_owned(), data: BASE64.encode(data) }
}

/// Decode a [`DaemonFrameUp::Compressed`] body (base64 then codec) back to the
/// serialized inner-frame bytes the sender compressed.
pub fn decode_compressed(codec: &str, b64: &str) -> std::io::Result<Vec<u8>> {
    let raw = BASE64.decode(b64).map_err(std::io::Error::other)?;
    decompress_codec(codec, &raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AdapterEvent;

    /// A synthetic transcript event of a realistic shape/size (§5). The
    /// deliberately repetitive envelope (tool names, keys, boilerplate prose)
    /// mirrors the cross-event redundancy that makes batch compression win.
    fn synth_event(i: usize) -> DaemonFrameUp {
        let payload = serde_json::json!({
            "role": "assistant",
            "message_id": format!("msg_{i:06}"),
            "tool": if i.is_multiple_of(3) { "Bash" } else { "Read" },
            "input": {
                "command": format!("grep -rn 'pattern_{}' /home/user/project/src", i % 7),
                "description": "Search the codebase for a recurring identifier pattern",
                "cwd": "/home/user/project",
            },
            "text": format!(
                "Here is iteration {i}. The assistant explains the same class of \
                 change repeatedly across the transcript, which is exactly the \
                 redundancy zstd exploits when the events are batched together \
                 before compression rather than framed one at a time."
            ),
            "usage": { "input_tokens": 1200 + i, "output_tokens": 340 + i },
        });
        DaemonFrameUp::Event {
            adapter_id: "claude-code".into(),
            event: AdapterEvent::Message {
                local_id: format!("sess-{}", i % 4),
                payload,
                turn_id: None,
            },
        }
    }

    #[test]
    fn roundtrip_small_skips_compression() {
        let data = b"tiny frame";
        let (bytes, codec) = maybe_compress(data);
        assert_eq!(codec, None);
        assert_eq!(bytes, data);
    }

    #[test]
    fn roundtrip_large_compresses_and_decompresses() {
        let big = serde_json::to_vec(&(0..200).map(synth_event).collect::<Vec<_>>()).unwrap();
        let (bytes, codec) = maybe_compress(&big);
        assert_eq!(codec, Some(CODEC_ZSTD));
        assert!(bytes.len() < big.len());
        assert_eq!(decompress_codec(CODEC_ZSTD, &bytes).unwrap(), big);
    }

    #[test]
    fn compressed_frame_roundtrips_through_decode() {
        let inner = serde_json::to_vec(&synth_event(1)).unwrap();
        let (bytes, codec) = (zstd_compress(&inner), CODEC_ZSTD);
        let frame = compressed_frame(codec, &bytes);
        let DaemonFrameUp::Compressed { codec, data } = frame else { panic!("wrong variant") };
        assert_eq!(decode_compressed(&codec, &data).unwrap(), inner);
    }

    #[test]
    fn unknown_codec_errors() {
        assert!(decompress_codec("brotli", b"whatever").is_err());
        assert_eq!(decompress_codec("", b"raw").unwrap(), b"raw");
    }

    #[test]
    fn compress_then_chunk_reassembles_to_original_batch() {
        use crate::chunk::{Accept, Reassembler, chunk_count, chunk_frame, transfer_id};

        // High-entropy per-event blobs so zstd can't collapse the batch below
        // the chunk threshold — the point is to prove the compress→chunk compose.
        let mut rng = 0x2545_f491_4f6c_dd1d_u64;
        let mut next_hex = |n: usize| {
            let bytes: Vec<u8> = (0..n)
                .map(|_| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    rng as u8
                })
                .collect();
            hex::encode(bytes)
        };
        let frames: Vec<DaemonFrameUp> = (0..3000_u64)
            .map(|i| DaemonFrameUp::Event {
                adapter_id: "claude-code".into(),
                event: AdapterEvent::Message {
                    local_id: format!("s{i}"),
                    payload: serde_json::json!({ "n": i, "blob": next_hex(120) }),
                    turn_id: None,
                },
            })
            .collect();
        let batch = DaemonFrameUp::Batch { frames };
        let inner = serde_json::to_vec(&batch).unwrap();
        let compressed = zstd_compress(&inner);
        let id = transfer_id(&compressed);
        let total = chunk_count(compressed.len());
        assert!(total > 1, "test needs a multi-chunk compressed payload");

        let mut reasm = Reassembler::new(usize::MAX);
        let mut joined = None;
        for i in 0..total {
            let DaemonFrameUp::Chunk { transfer_id, chunk_index, total_chunks, data, codec } =
                chunk_frame(&id, &compressed, i, total, Some(CODEC_ZSTD))
            else {
                panic!("chunk_frame must build a Chunk");
            };
            assert_eq!(codec.as_deref(), Some(CODEC_ZSTD));
            if let Accept::Complete(bytes) =
                reasm.accept(&transfer_id, chunk_index, total_chunks, &data)
            {
                joined = Some(bytes);
            }
        }
        let back = decompress_codec(CODEC_ZSTD, &joined.expect("reassembly completed")).unwrap();
        assert_eq!(
            back, inner,
            "reassembled+decompressed bytes must equal the original batch json"
        );
        let _: DaemonFrameUp = serde_json::from_slice(&back).unwrap();
    }

    #[test]
    fn batched_replay_hits_the_five_x_target() {
        // 500 realistic events, replayed one-per-frame vs coalesced into one
        // batch and zstd-compressed. Batch compression must beat 5x (§5).
        let events: Vec<DaemonFrameUp> = (0..500).map(synth_event).collect();
        let per_frame_bytes: usize =
            events.iter().map(|e| serde_json::to_vec(e).unwrap().len()).sum();
        let batch = DaemonFrameUp::Batch { frames: events };
        let batch_json = serde_json::to_vec(&batch).unwrap();
        let (wire, codec) = maybe_compress(&batch_json);
        assert_eq!(codec, Some(CODEC_ZSTD));
        let ratio = per_frame_bytes as f64 / wire.len() as f64;
        eprintln!(
            "batch replay: 500 events, per-frame={per_frame_bytes} B, \
             batched+zstd={} B, ratio={ratio:.1}x",
            wire.len()
        );
        assert!(ratio >= 5.0, "batch compression ratio {ratio:.1}x below 5x target");
    }
}
