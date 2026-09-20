//! Files the user sent (`paste-N.txt` masks, screenshots, docs), whether in the
//! spawn modal or mid-chat.
//!
//! `POST /sessions/spawn` and `POST /sessions/{id}/files` both, through
//! [`record_uploads`], keep a copy in the content-addressed blob store with a
//! `session_attachments` row per file, before the bytes are staged on the
//! daemon. `GET /api/v1/sessions/{id}/attachments`
//! lists those rows (session-read authz via the `api_router` layer); the bytes
//! come from the existing `GET /sessions/{id}/blobs/{hash}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use cctui_proto::media::sniff_media_type;
use serde::Serialize;

use crate::routes::blobs::store_blob;
use crate::state::AppState;
use crate::uploads::RawUpload;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SessionAttachment {
    pub id: uuid::Uuid,
    pub session_id: String,
    pub message_id: Option<String>,
    pub name: String,
    pub hash: String,
    pub size: i64,
    pub content_type: Option<String>,
    #[sqlx(rename = "created_at_ms")]
    pub created_at: i64,
    /// The session's machine, once it has registered: what the webui needs to
    /// fall back to the staged copy through `/machines/{id}/fs/file`.
    pub machine_id: Option<String>,
}

fn media_type_for(upload: &RawUpload) -> String {
    upload
        .content_type
        .as_deref()
        .map(|ct| ct.split(';').next().unwrap_or(ct).trim())
        .filter(|ct| !ct.is_empty() && *ct != "application/octet-stream")
        .map_or_else(|| sniff_media_type(&upload.name, &upload.bytes).to_owned(), str::to_owned)
}

/// Store each upload as a blob and record it against `session_id`.
/// `staged_names` are the daemon's final (collision-suffixed) names, index
/// aligned with `uploads`; a missing entry falls back to the upload name.
pub async fn record_uploads(
    pool: &sqlx::PgPool,
    session_id: &str,
    uploads: &[RawUpload],
    staged_names: &[String],
) -> Result<Vec<SessionAttachment>, sqlx::Error> {
    let mut out = Vec::with_capacity(uploads.len());
    for (i, upload) in uploads.iter().enumerate() {
        let media_type = media_type_for(upload);
        let stored = store_blob(pool, &upload.bytes, Some(&media_type)).await?;
        let name = staged_names.get(i).map_or(upload.name.as_str(), String::as_str);
        let row: SessionAttachment = sqlx::query_as(
            "INSERT INTO session_attachments (session_id, name, hash, size, content_type) \
             VALUES ($1, $2, $3, $4, $5) \
             RETURNING id, session_id, message_id, name, hash, size, content_type, \
                       (extract(epoch FROM created_at) * 1000)::bigint AS created_at_ms, \
                       (SELECT s.machine_id FROM sessions s WHERE s.id = $1) AS machine_id",
        )
        .bind(session_id)
        .bind(name)
        .bind(&stored.hash)
        .bind(i64::try_from(upload.bytes.len()).unwrap_or(i64::MAX))
        .bind(&media_type)
        .fetch_one(pool)
        .await?;
        out.push(row);
    }
    Ok(out)
}

/// Undo a [`record_uploads`] batch whose operation then failed. Blobs stay:
/// they are content-addressed and may be shared with another attachment.
pub async fn delete_attachments(
    pool: &sqlx::PgPool,
    ids: &[uuid::Uuid],
) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM session_attachments WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(())
}

/// Adopt the daemon's final (collision-suffixed) staged names. The conversation
/// matches chips to the paths printed under the message, so the row has to carry
/// the staged name — which is only known once staging has succeeded.
pub async fn set_attachment_names(
    pool: &sqlx::PgPool,
    rows: &[SessionAttachment],
    staged_names: &[String],
) -> Result<(), sqlx::Error> {
    for (row, staged) in rows.iter().zip(staged_names) {
        if row.name == *staged {
            continue;
        }
        sqlx::query("UPDATE session_attachments SET name = $2 WHERE id = $1")
            .bind(row.id)
            .bind(staged)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn list_session_attachments(
    pool: &sqlx::PgPool,
    session_id: &str,
) -> Result<Vec<SessionAttachment>, sqlx::Error> {
    sqlx::query_as(
        "SELECT a.id, a.session_id, a.message_id, a.name, a.hash, a.size, a.content_type, \
                (extract(epoch FROM a.created_at) * 1000)::bigint AS created_at_ms, \
                s.machine_id \
         FROM session_attachments a LEFT JOIN sessions s ON s.id = a.session_id \
         WHERE a.session_id = $1 ORDER BY a.created_at, a.id",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
}

pub async fn get_session_attachments(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<SessionAttachment>>, StatusCode> {
    list_session_attachments(&state.pool, &session_id).await.map(Json).map_err(|e| {
        tracing::error!(%session_id, "attachments list: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::{Action, Authn, Authz, IdFrom, ResourceKind};
    use axum::body::Bytes;
    use axum::http::Method;
    use uuid::Uuid;

    fn upload(name: &str, bytes: &[u8], content_type: Option<&str>) -> RawUpload {
        RawUpload {
            name: name.to_owned(),
            bytes: Bytes::copy_from_slice(bytes),
            content_type: content_type.map(str::to_owned),
        }
    }

    #[test]
    fn media_type_prefers_the_part_header_then_sniffs() {
        assert_eq!(
            media_type_for(&upload("a.txt", b"hi", Some("text/plain; charset=utf-8"))),
            "text/plain"
        );
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert_eq!(media_type_for(&upload("shot.png", &png, None)), "image/png");
        assert_eq!(
            media_type_for(&upload("shot.png", &png, Some("application/octet-stream"))),
            "image/png"
        );
    }

    #[test]
    fn listing_requires_session_read() {
        let descs = crate::build_api_routes().into_parts().1;
        let d = descs
            .iter()
            .find(|d| d.path == "/sessions/{id}/attachments" && d.method == Method::GET)
            .expect("attachments route registered");
        assert_eq!(d.authn, Authn::Bearer);
        assert!(
            matches!(
                d.authz,
                Authz::Resource(ResourceKind::Session, Action::Read, IdFrom::Path("id"))
            ),
            "attachments listing must be session-read scoped"
        );
    }

    #[tokio::test]
    async fn record_then_list_and_cascade_with_session() {
        let Some(url) = crate::routes::gateway::test_db_url("record_then_list") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");
        let uid = Uuid::new_v4();
        let machine = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, 'att-test', $2)")
            .bind(uid)
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO machines (id, user_id, name, key_hash) VALUES ($1, $2, $3, $4)")
            .bind(machine)
            .bind(uid)
            .bind(machine.to_string())
            .bind(format!("kh-{machine}"))
            .execute(&pool)
            .await
            .unwrap();
        let sid = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, working_dir, user_id, machine_uuid, adapter_id) \
             VALUES ($1, $2, '/w', $3, $4, 'claude-code')",
        )
        .bind(&sid)
        .bind(machine.to_string())
        .bind(uid)
        .bind(machine)
        .execute(&pool)
        .await
        .unwrap();

        let paste = format!("cct893 paste {sid}");
        let uploads = [
            upload("paste-1.txt", paste.as_bytes(), Some("text/plain")),
            upload("shot.png", &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1], None),
        ];
        let staged = [
            format!("/tmp/cctui-uploads/{sid}/paste-1.txt"),
            format!("/tmp/cctui-uploads/{sid}/shot-1.png"),
        ];
        let names: Vec<String> =
            staged.iter().map(|p| p.rsplit('/').next().unwrap().to_owned()).collect();
        let recorded = record_uploads(&pool, &sid, &uploads, &names).await.unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[1].name, "shot-1.png", "the daemon's suffixed name wins");

        let listed = list_session_attachments(&pool, &sid).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "paste-1.txt");
        assert_eq!(listed[0].content_type.as_deref(), Some("text/plain"));
        assert_eq!(listed[0].size, i64::try_from(paste.len()).unwrap());
        assert_eq!(listed[1].content_type.as_deref(), Some("image/png"));
        assert!(listed[0].created_at > 0);

        let (blob_len,): (i64,) =
            sqlx::query_as("SELECT byte_len FROM daemon_blobs WHERE hash = $1")
                .bind(&listed[0].hash)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(blob_len, listed[0].size);

        sqlx::query("DELETE FROM sessions WHERE id = $1").bind(&sid).execute(&pool).await.unwrap();
        assert!(list_session_attachments(&pool, &sid).await.unwrap().is_empty());
        for a in &listed {
            sqlx::query("DELETE FROM daemon_blobs WHERE hash = $1")
                .bind(&a.hash)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM machines WHERE id = $1")
            .bind(machine)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn records_before_the_session_row_exists_and_still_cascades() {
        let Some(url) = crate::routes::gateway::test_db_url("record_before_session") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");
        let uid = Uuid::new_v4();
        let machine = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, 'att-spawn', $2)")
            .bind(uid)
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO machines (id, user_id, name, key_hash) VALUES ($1, $2, $3, $4)")
            .bind(machine)
            .bind(uid)
            .bind(machine.to_string())
            .bind(format!("kh-{machine}"))
            .execute(&pool)
            .await
            .unwrap();

        // The spawn path records under the pre-minted session id, before the
        // worker has registered and created the `sessions` row.
        let sid = Uuid::new_v4().to_string();
        let body = format!("cct916 bootstrap {sid}");
        let uploads = [upload("paste-1.txt", body.as_bytes(), Some("text/plain"))];
        let recorded = record_uploads(&pool, &sid, &uploads, &[]).await.unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].name, "paste-1.txt", "no staged name yet: the upload name stands");

        let listed = list_session_attachments(&pool, &sid).await.unwrap();
        assert_eq!(listed.len(), 1);
        let hash = listed[0].hash.clone();
        let (blob_len,): (i64,) =
            sqlx::query_as("SELECT byte_len FROM daemon_blobs WHERE hash = $1")
                .bind(&hash)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(blob_len, i64::try_from(body.len()).unwrap());

        sqlx::query(
            "INSERT INTO sessions (id, machine_id, working_dir, user_id, machine_uuid, adapter_id) \
             VALUES ($1, $2, '/w', $3, $4, 'claude-code')",
        )
        .bind(&sid)
        .bind(machine.to_string())
        .bind(uid)
        .bind(machine)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM sessions WHERE id = $1").bind(&sid).execute(&pool).await.unwrap();
        assert!(
            list_session_attachments(&pool, &sid).await.unwrap().is_empty(),
            "the delete trigger replaces the dropped FK cascade"
        );

        sqlx::query("DELETE FROM daemon_blobs WHERE hash = $1")
            .bind(&hash)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM machines WHERE id = $1")
            .bind(machine)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn delete_attachments_undoes_a_recorded_batch() {
        let Some(url) = crate::routes::gateway::test_db_url("delete_attachments") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");
        let sid = Uuid::new_v4().to_string();
        let body = format!("cct1056 rollback {sid}");
        let uploads = [upload("note.txt", body.as_bytes(), Some("text/plain"))];
        let recorded = record_uploads(&pool, &sid, &uploads, &[]).await.unwrap();
        let ids: Vec<Uuid> = recorded.iter().map(|a| a.id).collect();

        set_attachment_names(&pool, &recorded, &["note-1.txt".to_owned()]).await.unwrap();
        assert_eq!(list_session_attachments(&pool, &sid).await.unwrap()[0].name, "note-1.txt");

        delete_attachments(&pool, &ids).await.unwrap();
        assert!(list_session_attachments(&pool, &sid).await.unwrap().is_empty());

        sqlx::query("DELETE FROM daemon_blobs WHERE hash = $1")
            .bind(&recorded[0].hash)
            .execute(&pool)
            .await
            .unwrap();
    }
}
