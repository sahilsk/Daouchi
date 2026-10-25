use crate::api::AppState;
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader};
use tokio_util::io::ReaderStream;

#[derive(Deserialize)]
struct StreamQuery {
    path: String,
}

async fn stream_audio(
    State(state): State<Arc<AppState>>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<Response<Body>, (StatusCode, String)> {
    // Security: canonicalize and verify against indexed roots
    let canonical = tokio::fs::canonicalize(&q.path)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "file not found".to_string()))?;

    let canonical_str = canonical.to_string_lossy().to_string();

    // App-generated stem files always allowed (no user data can be there).
    let stems_cache = crate::lifecycle::stems_cache_dir()
        .to_string_lossy()
        .into_owned();
    let is_stems_cache = canonical_str.starts_with(&format!("{}/", stems_cache));

    let allowed = if is_stems_cache {
        true
    } else {
        let roots = state.indexed_roots.lock().unwrap().clone();
        if roots.is_empty() {
            // indexed_roots table not yet populated (pre-existing library).
            // Fall back to checking the sounds table — if the file is already
            // indexed it is safe to stream.
            let conn = state.pool.get();
            let in_sounds: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sounds WHERE path = ?1",
                    rusqlite::params![canonical_str],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let in_roots: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM indexed_roots \
                     WHERE SUBSTR(?1, 1, LENGTH(path) + 1) = path || '/'
                        OR ?1 = path",
                    rusqlite::params![canonical_str],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            in_sounds > 0 || in_roots > 0
        } else {
            roots.iter().any(|root| {
                canonical_str == root.as_str()
                    || canonical_str.starts_with(&format!("{}/", root))
            })
        }
    };

    if !allowed {
        return Err((StatusCode::FORBIDDEN, "access denied".to_string()));
    }

    // Get file size
    let file_size = tokio::fs::metadata(&canonical)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "file not found".to_string()))?
        .len();

    let mime = mime_guess::from_path(&canonical)
        .first_or_octet_stream()
        .to_string();

    // Parse Range header
    if let Some(range_val) = headers.get(header::RANGE) {
        let range_str = range_val.to_str().unwrap_or("");
        match parse_range(range_str, file_size) {
            Some((start, end)) => {
                // Clamp oversized ranges to MAX_CHUNK bytes — the client (WaveSurfer)
                // will issue follow-up range requests for the remainder.
                // Returning 416 for a satisfiable-but-large range violates RFC 7233.
                const MAX_CHUNK: u64 = 16 * 1024 * 1024; // 16 MiB
                let requested_len = end - start + 1;
                let length = requested_len.min(MAX_CHUNK);
                let actual_end = start + length - 1;

                let mut file = tokio::fs::File::open(&canonical)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

                let mut buf = vec![0u8; length as usize];
                file.read_exact(&mut buf)
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

                return Ok(Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, mime)
                    .header(
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{actual_end}/{file_size}"),
                    )
                    .header(header::CONTENT_LENGTH, length.to_string())
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(Body::from(buf))
                    .unwrap());
            }
            None => {
                // A Range header was present but unsatisfiable or malformed — RFC 7233
                // requires 416 Range Not Satisfiable, not a fallthrough to 200.
                return Err((
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    format!("bytes */{file_size}"),
                ));
            }
        }
    }

    // No Range header — stream the full file to avoid unbounded heap allocation.
    // Using BufReader + ReaderStream keeps memory bounded regardless of file size.
    let file = tokio::fs::File::open(&canonical)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let stream = ReaderStream::new(BufReader::new(file));

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::from_stream(stream))
        .unwrap())
}

/// Parse `bytes=START-END` or `bytes=START-`. Returns `(start, end)` inclusive.
fn parse_range(s: &str, file_size: u64) -> Option<(u64, u64)> {
    let s = s.strip_prefix("bytes=")?;
    let mut parts = s.splitn(2, '-');
    let start_str = parts.next()?;
    let end_str = parts.next().unwrap_or("");

    let start: u64 = start_str.parse().ok()?;
    let end: u64 = if end_str.is_empty() {
        file_size.saturating_sub(1)
    } else {
        end_str.parse().ok()?
    };

    if start > end || end >= file_size {
        return None;
    }

    Some((start, end))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/audio/stream", get(stream_audio))
}
