//! Daouchi application library. The Tauri desktop shell (`crates/daouchi-desktop`) builds the axum
//! [`Router`](axum::Router) with [`build_app`] and dispatches webview requests into it in-process
//! (see [`transport`]). There is no TCP listener anywhere: no socket is ever bound.
pub mod api;
pub mod db;
pub mod db_migrate;
pub mod engines;
#[cfg(test)]
mod integ_tests;
pub mod indexer;
pub mod lifecycle;
pub mod transport;

use anyhow::Result;
use api::AppState;
use std::sync::{Arc, Mutex};

/// The assembled application: the request router plus the shared state behind it.
pub struct App {
    pub router: axum::Router,
    pub state: Arc<AppState>,
}

/// Initialise storage (`DAOUCHI_HOME`, default `~/.daouchi`), database and engines and build the
/// router serving the API + embedded web UI. Binds nothing. Must be called from within a tokio runtime
/// (engines spawn background tasks).
pub fn build_app() -> Result<App> {
    let home = lifecycle::init()?.clone();
    tracing::info!("Daouchi home: {}", home.root().display());

    let db_path = lifecycle::db_path();
    lifecycle::migrate_legacy_db(&db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let pool = db::open(&db_path)?;
    db_migrate::init_and_migrate(&*pool.get())?;

    // In-process engines. No models are required to start; each feature downloads its model on demand.
    let ai = engines::AiEngines::new(home, pool.clone());

    // Failure here is fatal: without indexed_roots the path-security check in audio.rs cannot work.
    let indexed_roots: Vec<String> = {
        let conn = pool.get();
        let mut stmt = conn
            .prepare("SELECT path FROM indexed_roots")
            .map_err(|e| anyhow::anyhow!("failed to read indexed_roots table: {e}"))?;
        let roots: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(|e| anyhow::anyhow!("failed to iterate indexed_roots: {e}"))?
            .flatten()
            .collect();
        roots
    };

    let state = Arc::new(AppState { pool, ai, indexed_roots: Arc::new(Mutex::new(indexed_roots)) });
    let router = api::build_router(state.clone());
    Ok(App { router, state })
}
