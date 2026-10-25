//! Model management endpoints: list, download, SSE status events, bootstrap status.
use crate::api::{ApiError, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;

async fn list_models(State(state): State<Arc<AppState>>) -> Json<Vec<crate::engines::ModelInfo>> {
    Json(state.ai.model_status().await)
}

/// Idempotent: starts a background download (if not ready / not already running); 202 + status list.
async fn download_model(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let infos = state.ai.ensure_model(&name).await?;
    Ok((StatusCode::ACCEPTED, Json(infos)))
}

/// SSE: one event on connect, then one per change. Each event's data is the same JSON array as GET /api/models.
async fn model_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = state.ai.subscribe_models();
    let ai = state.ai.clone();
    let stream = async_stream::stream! {
        let first = serde_json::to_string(&ai.model_status().await).unwrap_or_default();
        yield Ok(Event::default().data(first));
        loop {
            match rx.recv().await {
                Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // coalesce bursts
                    while rx.try_recv().is_ok() {}
                    let body = serde_json::to_string(&ai.model_status().await).unwrap_or_default();
                    yield Ok(Event::default().data(body));
                }
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// For web/bootstrap.html. AI models are optional, so the app is always usable: status is "done"
/// unless a download is in progress.
async fn bootstrap_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let infos = state.ai.model_status().await;
    let mb = |b: u64| (b as f64 / 1_048_576.0).round() as u64;
    let any_dl = infos.iter().any(|m| m.downloading.is_some());
    let models: Vec<_> = infos
        .iter()
        .map(|m| {
            let (done, progress) = match (&m.downloading, m.status) {
                (_, "ready") => (m.total_bytes, 1.0),
                (Some(d), _) if d.total_bytes > 0 => (d.done_bytes, d.done_bytes as f64 / d.total_bytes as f64),
                _ => (0, 0.0),
            };
            serde_json::json!({
                "name": m.label,
                "id": m.name,
                "downloaded_mb": mb(done),
                "total_mb": mb(m.total_bytes),
                "progress": progress,
            })
        })
        .collect();
    Json(serde_json::json!({ "status": if any_dl { "downloading" } else { "done" }, "models": models }))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/models", get(list_models))
        .route("/api/models/events", get(model_events))
        .route("/api/models/:name/download", post(download_model))
        .route("/api/bootstrap/status", get(bootstrap_status))
}
