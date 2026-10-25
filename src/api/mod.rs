use crate::db::DbPool;
use crate::engines::{AiEngines, AiError};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::sync::{Arc, Mutex};

pub mod assets;
pub mod audio;
pub mod export;
pub mod index;
pub mod library;
pub mod metadata;
pub mod models;
pub mod mood;
pub mod projects;
pub mod radar;
pub mod search;
pub mod stems;

/// Shared application state threaded through all axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub pool: DbPool,
    pub ai: Arc<AiEngines>,
    pub indexed_roots: Arc<Mutex<Vec<String>>>,
}

/// Handler error: status + JSON body. Converts from the legacy `(StatusCode, String)` tuple
/// (plain-text body) and from `AiError` (JSON; missing model => 409 `model_not_ready`).
pub struct ApiError(pub StatusCode, pub Option<serde_json::Value>, pub String);

impl From<(StatusCode, String)> for ApiError {
    fn from((c, m): (StatusCode, String)) -> Self {
        ApiError(c, None, m)
    }
}

impl From<AiError> for ApiError {
    fn from(e: AiError) -> Self {
        match e {
            AiError::ModelNotReady(m) => ApiError(
                StatusCode::CONFLICT,
                Some(serde_json::json!({ "error": "model_not_ready", "model": m })),
                String::new(),
            ),
            AiError::Unavailable(s) => ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                Some(serde_json::json!({ "error": "engine_unavailable", "detail": s })),
                String::new(),
            ),
            AiError::BadRequest(s) => ApiError(StatusCode::BAD_REQUEST, None, s),
            AiError::Other(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, None, format!("{e:#}")),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self.1 {
            Some(v) => (self.0, Json(v)).into_response(),
            None => (self.0, self.2).into_response(),
        }
    }
}

pub fn build_router(state: Arc<AppState>) -> axum::Router {
    use axum::Router;
    use tower_http::set_header::SetResponseHeaderLayer;
    use axum::http::{header, HeaderValue};

    Router::new()
        .merge(index::router())
        .merge(search::router())
        .merge(audio::router())
        .merge(metadata::router())
        .merge(library::router())
        .merge(models::router())
        .merge(stems::router())
        .merge(export::router())
        .merge(radar::router())
        .merge(mood::router())
        .merge(projects::router())
        // Web UI (embedded; DAOUCHI_WEB_DIR overrides). no-cache layer below keeps dev edits visible.
        .fallback(assets::serve)
        .with_state(state)
        // No CORS layer: the UI is same-origin (daouchi:// custom scheme), so cross-origin pages get nothing.
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, no-store, must-revalidate"),
        ))
}
