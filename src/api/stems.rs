use crate::api::AppState;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
struct StemsStatusResponse {
    status: String,
    stems: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Public alias used by export.rs for stem export.
pub fn stems_output_dir_pub(file_path: &str) -> String {
    stems_output_dir(file_path)
}

/// Derive the stems output directory for a given file path.
fn stems_output_dir(file_path: &str) -> String {
    let hash = blake3::hash(file_path.as_bytes()).to_hex();
    crate::lifecycle::stems_cache_dir()
        .join(hash.as_str())
        .to_string_lossy()
        .to_string()
}

async fn get_stems_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<StemsStatusResponse>, (axum::http::StatusCode, String)> {
    let path: String = {
        let conn = state.pool.get();
        conn.query_row("SELECT path FROM sounds WHERE id = ?", [id], |r| r.get(0))
            .map_err(|_| (axum::http::StatusCode::NOT_FOUND, "not found".to_string()))?
    };

    let output_dir = stems_output_dir(&path);

    // Fast path: all four stem files are already on disk (e.g. from an earlier build).
    // Run the blocking dir/file existence checks on a spawn_blocking thread so we
    // do not stall the async executor on slow or network-mounted filesystems.
    let output_dir_clone = output_dir.clone();
    let stems_result = tokio::task::spawn_blocking(move || {
        let dir = std::path::Path::new(&output_dir_clone);
        if dir.exists() {
            const EXPECTED_STEMS: &[&str] = &["drums", "bass", "vocals", "other"];
            let mut stems_map = serde_json::Map::new();
            for &stem in EXPECTED_STEMS {
                let p = dir.join(format!("{stem}.wav"));
                if p.is_file() {
                    stems_map.insert(
                        stem.to_string(),
                        serde_json::Value::String(p.to_string_lossy().to_string()),
                    );
                }
            }
            if stems_map.len() == EXPECTED_STEMS.len() {
                return Some(stems_map);
            }
        }
        None
    })
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(stems_map) = stems_result {
        return Ok(Json(StemsStatusResponse {
            status: "done".to_string(),
            stems: Some(serde_json::Value::Object(stems_map)),
            error: None,
            reason: None,
        }));
    }
    // Stems are not part of this build (no separation engine yet).
    let _ = id;
    Ok(Json(StemsStatusResponse {
        status: "not_available".to_string(),
        stems: None,
        error: None,
        reason: Some(NOT_AVAILABLE_REASON.to_string()),
    }))
}

const NOT_AVAILABLE_REASON: &str = "stems not included in this build";

async fn split_stems(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    {
        let conn = state.pool.get();
        conn.query_row("SELECT 1 FROM sounds WHERE id = ?", [id], |r| r.get::<_, i64>(0))
            .map_err(|_| (axum::http::StatusCode::NOT_FOUND, "not found".to_string()))?;
    }
    Ok(Json(serde_json::json!({ "status": "not_available", "reason": NOT_AVAILABLE_REASON })))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/stems/:id", get(get_stems_status))
        .route("/api/stems/:id/split", post(split_stems))
        .route("/api/stems/:id/reveal", post(reveal_stems_folder))
        .route("/api/stems/:id/save", post(save_stems))
}

/// Open the stems cache directory in macOS Finder.
async fn reveal_stems_folder(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let path: String = {
        let conn = state.pool.get();
        conn.query_row("SELECT path FROM sounds WHERE id = ?", [id], |r| r.get(0))
            .map_err(|_| (axum::http::StatusCode::NOT_FOUND, "not found".to_string()))?
    };
    let cache_dir = stems_output_dir(&path);
    if !std::path::Path::new(&cache_dir).is_dir() {
        return Err((axum::http::StatusCode::NOT_FOUND, "no stems have been generated for this sound yet".to_string()));
    }
    let _ = std::process::Command::new("open").arg(&cache_dir).spawn();
    Ok(Json(serde_json::json!({ "path": cache_dir })))
}

/// Copy stems from cache to {original_dir}/{basename}_stems/, then reveal in Finder.
async fn save_stems(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let sound_path_str: String = {
        let conn = state.pool.get();
        conn.query_row("SELECT path FROM sounds WHERE id = ?", [id], |r| r.get(0))
            .map_err(|_| (axum::http::StatusCode::NOT_FOUND, "not found".to_string()))?
    };

    let sound_path = std::path::Path::new(&sound_path_str);
    let sound_dir = sound_path.parent().ok_or_else(|| {
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "invalid path".to_string())
    })?;
    let basename = sound_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "stems".to_string());

    let dest_dir = sound_dir.join(format!("{basename}_stems"));
    tokio::fs::create_dir_all(&dest_dir)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let cache_dir = std::path::PathBuf::from(stems_output_dir(&sound_path_str));
    let mut copied = 0usize;
    for stem in ["drums", "bass", "vocals", "other"] {
        let src = cache_dir.join(format!("{stem}.wav"));
        // Use async try_exists to avoid blocking the executor on a potentially
        // slow or network-mounted cache directory.
        match tokio::fs::try_exists(&src).await {
            Ok(true) => {
                tokio::fs::copy(&src, dest_dir.join(format!("{stem}.wav")))
                    .await
                    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                copied += 1;
            }
            Ok(false) => {} // stem not present yet — skip
            Err(e) => return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        }
    }

    if copied == 0 {
        return Ok(Json(serde_json::json!({
            "success": false,
            "error": "No stem files found in cache — run Split Stems first"
        })));
    }

    // Reveal in Finder
    let dest_str = dest_dir.to_string_lossy().to_string();
    let _ = std::process::Command::new("open").arg(&dest_str).spawn();

    Ok(Json(serde_json::json!({ "success": true, "path": dest_str, "copied": copied })))
}
