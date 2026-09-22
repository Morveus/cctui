//! Marker-based image posting.
//!
//! An agent shows a picture by writing a plain markdown image marker into its
//! message text — `![alt](/abs/path.png)`, a local absolute path with an image
//! extension. The daemon detects that marker in an *assistant* message, reads
//! the file (under an allow-list, size-capped), uploads the bytes to the server
//! blob store, and rewrites the marker to `![alt](cctui-img://<image_id>)` so it
//! rides the existing text payload unchanged through the DB/replay/WS. The webui
//! renders that `cctui-img://` scheme inline. On any failure the marker is left
//! exactly as the agent wrote it (degrades to today's behaviour — a plain path).

use std::path::{Path, PathBuf};

use cctui_proto::adapter::AdapterEvent;

use crate::client::ServerClient;

/// Per-image byte cap, mirroring the server's `MAX_IMAGE_BYTES`.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// One `![alt](path)` marker located in message text, as a byte span over the
/// source plus its parts.
#[derive(Debug, PartialEq, Eq)]
struct Marker {
    start: usize,
    end: usize,
    alt: String,
    path: String,
}

fn has_image_ext(path: &str) -> bool {
    let ext = Path::new(path).extension().and_then(|s| s.to_str()).map(str::to_ascii_lowercase);
    matches!(ext.as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp"))
}

/// Sniff an image media type from magic bytes, so the upload's `Content-Type`
/// is honest (the server re-sniffs authoritatively). `None` = not one of the
/// allowed types.
fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Scan text for `![alt](path)` markers whose path is absolute with an image
/// extension. Hand-rolled (no regex dep): find `![`, the `]`, an immediate `(`,
/// and the closing `)`. Only absolute-path image markers are returned, so a
/// remote `![x](https://…)` link is ignored (the webui keeps escaping those).
fn find_markers(text: &str) -> Vec<Marker> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] != b'!' || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let alt_start = i + 2;
        let Some(rel_close) = text[alt_start..].find(']') else { break };
        let alt_end = alt_start + rel_close;
        if alt_end + 1 >= bytes.len() || bytes[alt_end + 1] != b'(' {
            i = alt_end + 1;
            continue;
        }
        let path_start = alt_end + 2;
        let Some(rel_paren) = text[path_start..].find(')') else { break };
        let path_end = path_start + rel_paren;
        let path = &text[path_start..path_end];
        if path.starts_with('/') && has_image_ext(path) {
            out.push(Marker {
                start: i,
                end: path_end + 1,
                alt: text[alt_start..alt_end].to_owned(),
                path: path.to_owned(),
            });
        }
        i = path_end + 1;
    }
    out
}

/// Default allow-list roots: the OS temp dir(s) and the user's home.
///
/// An image path must canonicalize (symlinks resolved) to a regular file under
/// one of these. This blocks references to system paths (`/etc`, `/proc`, …)
/// while covering the two places an agent writes screenshots — a scratch temp
/// file or a file inside its project (projects live under `$HOME`). The server's
/// magic-byte media-type sniff + per-session, owner-only serving are the harder
/// guards behind it.
#[must_use]
pub fn default_allowed_roots() -> Vec<PathBuf> {
    let mut roots =
        vec![std::env::temp_dir(), PathBuf::from("/tmp"), PathBuf::from("/private/tmp")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home);
    }
    roots.into_iter().filter_map(|r| r.canonicalize().ok()).collect()
}

/// Whether `path` is an allowed image file: it canonicalizes to a regular file
/// under one of `allowed_roots` (also canonicalized by the caller).
fn path_allowed(path: &str, allowed_roots: &[PathBuf]) -> bool {
    let Ok(canon) = Path::new(path).canonicalize() else { return false };
    if !canon.is_file() {
        return false;
    }
    allowed_roots.iter().any(|root| canon.starts_with(root))
}

/// Read + validate a marker's image file (allow-list, size cap, magic-byte
/// media type). `None` on any check failure, so the marker is left untouched.
fn read_marker_image(path: &str, allowed_roots: &[PathBuf]) -> Option<(Vec<u8>, &'static str)> {
    if !path_allowed(path, allowed_roots) {
        tracing::debug!(%path, "image marker path not in allow-list; leaving as-is");
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        tracing::debug!(%path, len = bytes.len(), "image over cap; leaving as-is");
        return None;
    }
    let media_type = sniff_media_type(&bytes)?;
    Some((bytes, media_type))
}

/// Read + upload one marker's image, returning the served `cctui-img://<id>`
/// ref. `None` on any failure so the caller leaves the marker untouched.
async fn upload_marker(
    client: &ServerClient,
    machine_key: &str,
    session_id: &str,
    marker: &Marker,
    allowed_roots: &[PathBuf],
) -> Option<String> {
    let (bytes, media_type) = read_marker_image(&marker.path, allowed_roots)?;
    match client.upload_session_image(machine_key, session_id, bytes, media_type).await {
        Ok(image_id) => Some(image_id),
        Err(err) => {
            tracing::warn!(path = %marker.path, %err, "image upload failed; leaving marker as-is");
            None
        }
    }
}

/// Rewrite every uploadable `![alt](/abs/path)` marker in `text` to
/// `![alt](cctui-img://<id>)`. Markers that fail (missing/over-cap/not an image/
/// upload error) are left verbatim.
pub async fn rewrite_text(
    client: &ServerClient,
    machine_key: &str,
    session_id: &str,
    text: &str,
    allowed_roots: &[PathBuf],
) -> String {
    let markers = find_markers(text);
    if markers.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for marker in &markers {
        out.push_str(&text[cursor..marker.start]);
        match upload_marker(client, machine_key, session_id, marker, allowed_roots).await {
            Some(image_id) => {
                out.push_str("![");
                out.push_str(&marker.alt);
                out.push_str("](cctui-img://");
                out.push_str(&image_id);
                out.push(')');
            }
            None => out.push_str(&text[marker.start..marker.end]),
        }
        cursor = marker.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Rewrite image markers in an assistant `Message`, else pass the event through.
///
/// This is the single choke point the supervisor's per-adapter event pump routes
/// every event through, so it is adapter-agnostic (claude + codex) and never
/// blocks the WS loop (the pump task is per-adapter).
pub async fn process_event(
    client: &ServerClient,
    machine_key: &str,
    event: AdapterEvent,
    allowed_roots: &[PathBuf],
) -> AdapterEvent {
    let event = crate::blobs::extract_blobs(client, machine_key, event).await;
    let AdapterEvent::Message { local_id, payload, .. } = &event else { return event };
    if payload.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return event;
    }
    let Some(text) = payload.get("text").and_then(|t| t.as_str()) else { return event };
    // Cheap reject before any parsing/IO.
    if !text.contains("![") {
        return event;
    }
    let rewritten = rewrite_text(client, machine_key, local_id, text, allowed_roots).await;
    if rewritten == text {
        return event;
    }
    let mut payload = payload.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("text".to_owned(), serde_json::Value::String(rewritten));
    }
    AdapterEvent::Message { local_id: local_id.clone(), payload, turn_id: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn finds_absolute_image_markers_only() {
        let text = "see ![a shot](/tmp/x.png) and ![remote](https://y.com/z.png) and \
                    ![rel](notes.png) and ![doc](/tmp/report.pdf)";
        let markers = find_markers(text);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].alt, "a shot");
        assert_eq!(markers[0].path, "/tmp/x.png");
        assert_eq!(&text[markers[0].start..markers[0].end], "![a shot](/tmp/x.png)");
    }

    #[test]
    fn finds_multiple_markers() {
        let text = "![one](/tmp/a.jpg)![two](/tmp/b.gif)";
        let markers = find_markers(text);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].path, "/tmp/a.jpg");
        assert_eq!(markers[1].path, "/tmp/b.gif");
    }

    #[test]
    fn empty_alt_ok() {
        let markers = find_markers("![](/tmp/x.webp)");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].alt, "");
    }

    #[test]
    fn no_markers_in_plain_text() {
        assert!(find_markers("just some text with a ! and [brackets] and (parens)").is_empty());
    }

    #[test]
    fn media_type_sniffing() {
        assert_eq!(
            sniff_media_type(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(sniff_media_type(&[0xFF, 0xD8, 0xFF]), Some("image/jpeg"));
        assert_eq!(sniff_media_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff_media_type(b"not an image"), None);
    }

    #[test]
    fn path_allowed_accepts_real_file_under_root_rejects_outside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let img = root.join("shot.png");
        let mut f = std::fs::File::create(&img).unwrap();
        f.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
        let roots = vec![root.clone()];
        assert!(path_allowed(img.to_str().unwrap(), &roots));
        // A path outside the root is rejected.
        assert!(!path_allowed("/etc/hosts", &roots));
        // A directory is not a file.
        assert!(!path_allowed(root.to_str().unwrap(), &roots));
        // A non-existent path is rejected.
        assert!(!path_allowed(root.join("nope.png").to_str().unwrap(), &roots));
    }

    #[test]
    fn rewrite_leaves_unresolvable_markers_untouched() {
        // No server call happens because the path fails the allow-list first, so
        // a dummy client is never exercised — assert the failure path keeps text.
        let markers = find_markers("![x](/tmp/does-not-exist-cct566.png)");
        assert_eq!(markers.len(), 1);
        assert!(!path_allowed("/tmp/does-not-exist-cct566.png", &default_allowed_roots()));
    }
}
