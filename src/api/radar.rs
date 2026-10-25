use crate::api::{ApiError, AppState};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
struct RadarStatus {
    /// "none" | "not_started" | "computing" | "ready" | "error"
    status: String,
    /// done = sounds embedded with the current model, total = all sounds
    progress: RadarProgress,
    /// "" | "embedding" | "umap" while computing
    phase: String,
    /// current-model embeddings
    embedded: i64,
    /// current-model embeddings that have radar coordinates
    mapped: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    job: crate::engines::RadarJob,
}

#[derive(Serialize)]
struct RadarProgress {
    done: i64,
    total: i64,
}

#[derive(Serialize)]
struct RadarPoint {
    sound_id: i64,
    x: f64,
    y: f64,
    filename: String,
    duration: Option<f64>,
    source: Option<String>,
    collection: Option<String>,
}

#[derive(Serialize)]
struct RadarPointsResponse {
    points: Vec<RadarPoint>,
}

#[derive(Serialize)]
struct ComputeResponse {
    status: String,
}

async fn radar_status(State(state): State<Arc<AppState>>) -> Json<RadarStatus> {
    let (total_sounds, embedded, coords) = {
        let conn = state.pool.get();
        let count = |sql: &str| -> i64 {
            conn.query_row(sql, [crate::engines::CURRENT_MODEL], |r| r.get(0)).unwrap_or(0)
        };
        (
            conn.query_row("SELECT COUNT(*) FROM sounds", [], |r| r.get(0)).unwrap_or(0i64),
            // only embeddings from the current model count (legacy rows are ignored)
            count("SELECT COUNT(*) FROM embeddings WHERE model = ?1"),
            count(
                "SELECT COUNT(*) FROM radar_coords r JOIN embeddings e ON e.sound_id = r.sound_id \
                 WHERE e.model = ?1",
            ),
        )
    };
    let job = state.ai.job();

    let status = if job.state == "running" {
        "computing"
    } else if job.state == "error" {
        "error"
    } else if total_sounds == 0 {
        "none"
    } else if coords == 0 {
        "not_started"
    } else {
        "ready"
    };

    Json(RadarStatus {
        status: status.to_string(),
        progress: RadarProgress { done: embedded, total: total_sounds },
        phase: job.phase.clone(),
        embedded,
        mapped: coords,
        error: job.error.clone().filter(|_| job.state == "error"),
        job,
    })
}

async fn radar_points(State(state): State<Arc<AppState>>) -> Result<Json<RadarPointsResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();

    let mut stmt = conn
        .prepare(
            "SELECT r.sound_id, r.x, r.y, s.filename, s.duration, s.source, s.collection \
             FROM radar_coords r \
             JOIN sounds s ON s.id = r.sound_id \
             JOIN embeddings e ON e.sound_id = r.sound_id AND e.model = ?1 \
             ORDER BY r.sound_id \
             LIMIT 10000",
        )
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let points: Vec<RadarPoint> = stmt
        .query_map([crate::engines::CURRENT_MODEL], |r| {
            Ok(RadarPoint {
                sound_id: r.get(0)?,
                x: r.get(1)?,
                y: r.get(2)?,
                filename: r.get(3)?,
                duration: r.get(4)?,
                source: r.get(5)?,
                collection: r.get(6)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();

    Ok(Json(RadarPointsResponse { points }))
}

/// UMAP only (over existing current-model embeddings). Runs in the background.
async fn compute_radar(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ComputeResponse>, ApiError> {
    let job = state.ai.start_umap().await?;
    Ok(Json(ComputeResponse { status: started_label(&job) }))
}

fn started_label(job: &crate::engines::RadarJob) -> String {
    if job.state == "running" { "started".to_string() } else { job.state.clone() }
}

/// Embed all sounds lacking a current-model embedding, then run UMAP.
/// Returns immediately; the frontend polls /api/radar/status.
async fn generate_radar(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ComputeResponse>, ApiError> {
    let job = state.ai.start_radar_generate().await?;
    Ok(Json(ComputeResponse { status: started_label(&job) }))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/radar/status", get(radar_status))
        .route("/api/radar/points", get(radar_points))
        .route("/api/radar/compute", post(compute_radar))
        .route("/api/radar/generate", post(generate_radar))
}
