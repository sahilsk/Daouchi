//! Socket-free end-to-end check of the router (tower oneshot). Runs only with DAOUCHI_INTEG=1 and
//! DAOUCHI_HOME pointing at a scratch home; phase 2 (DAOUCHI_INTEG_MODELS=1) expects models preseeded.
use crate::{api, db, db_migrate, engines, lifecycle};
use axum::{body::Body, http::Request};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower::ServiceExt;

fn db_id(_a: &axum::Router) -> i64 { let c = rusqlite::Connection::open(lifecycle::db_path()).unwrap(); c.query_row("SELECT id FROM sounds ORDER BY id LIMIT 1", [], |r| r.get(0)).unwrap_or(1) }

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<&str>) -> (u16, String) {
    let t = Instant::now();
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(s) => { b = b.header("content-type", "application/json"); Body::from(s.to_string()) }
        None => Body::empty(),
    };
    let r = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let st = r.status().as_u16();
    let bytes = axum::body::to_bytes(r.into_body(), 64 << 20).await.unwrap();
    let s = String::from_utf8_lossy(&bytes).to_string();
    let short: String = s.chars().take(300).collect();
    println!("{method} {uri} -> {st} [{:?}] {short}", t.elapsed());
    (st, s)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integration() {
    if std::env::var_os("DAOUCHI_INTEG").is_none() { return; }
    let home = lifecycle::init().unwrap().clone();
    let dbp = lifecycle::db_path();
    lifecycle::migrate_legacy_db(&dbp);
    let pool = db::open(&dbp).unwrap();
    db_migrate::init_and_migrate(&*pool.get()).unwrap();
    let ai = engines::AiEngines::new(home, pool.clone());
    let roots = Arc::new(Mutex::new(vec![]));
    let app = api::build_router(Arc::new(api::AppState { pool, ai, indexed_roots: roots }));

    let id: i64 = db_id(&app);
    call(&app, "GET", "/api/models", None).await;
    call(&app, "GET", "/api/bootstrap/status", None).await;
    call(&app, "GET", "/api/radar/status", None).await;
    call(&app, "GET", "/api/sounds/stats", None).await;
    call(&app, "GET", "/api/sounds/search?q=door&limit=3", None).await;
    call(&app, "GET", &format!("/api/sounds/similar/{ID}", ID = id), None).await;
    let (st, _) = call(&app, "POST", "/api/sounds/semantic_search", Some(r#"{"query":"door slam","limit":5}"#)).await;
    if std::env::var_os("DAOUCHI_INTEG_MODELS").is_none() {
        assert_eq!(st, 409);
        call(&app, "POST", "/api/radar/generate", None).await;
        call(&app, "POST", "/api/models/bogus/download", None).await;
        return;
    }
    assert_eq!(st, 200);
    call(&app, "POST", "/api/ai/mood_search", Some(r#"{"valence":0.5,"arousal":0.8,"limit":3}"#)).await;
    call(&app, "POST", "/api/radar/generate", None).await;
    for _ in 0..600 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let (_, s) = call(&app, "GET", "/api/radar/status", None).await;
        if !s.contains("computing") { break; }
    }
    call(&app, "GET", "/api/radar/points", None).await;
    call(&app, "POST", "/api/sounds/semantic_search", Some(r#"{"query":"footsteps on gravel","limit":5}"#)).await;
    call(&app, "GET", &format!("/api/sounds/similar/{ID}?limit=3", ID = id), None).await;
    call(&app, "GET", "/api/ai/sfx_category_counts", None).await;
    call(&app, "GET", &format!("/api/sounds/{ID}/suggested_tags", ID = id), None).await;
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    call(&app, "GET", &format!("/api/sounds/{ID}/suggested_tags", ID = id), None).await;
    for extra in [id + 1, id + 5, id + 20] {
        let _ = call(&app, "GET", &format!("/api/sounds/{}/suggested_tags", extra), None).await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    for extra in [id, id + 1, id + 5, id + 20] {
        call(&app, "GET", &format!("/api/sounds/{}/suggested_tags", extra), None).await;
    }
    call(&app, "GET", &format!("/api/sounds/{ID}/transients", ID = id), None).await;
    call(&app, "GET", &format!("/api/stems/{ID}", ID = id), None).await;
    call(&app, "POST", &format!("/api/stems/{ID}/split", ID = id), None).await;
}
