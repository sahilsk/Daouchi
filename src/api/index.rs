use crate::api::AppState;
use crate::indexer::{run_index, FolderConfig, IndexProgress};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    mpsc,
    Arc, Mutex,
};

#[derive(Deserialize)]
struct StartBody {
    folders: Vec<FolderInput>,
}

#[derive(Deserialize)]
struct FolderInput {
    path: String,
    source: String,
}

#[derive(Serialize)]
struct StartResponse {
    status: String,
}

/// Global SSE progress sender – only one indexing job runs at a time.
static PROGRESS_TX: std::sync::OnceLock<
    Arc<Mutex<Option<tokio::sync::broadcast::Sender<IndexProgress>>>>,
> = std::sync::OnceLock::new();

/// Latest progress snapshot, for `GET /api/index/status` (polling alternative to the SSE stream; the
/// desktop shell serves the UI over a custom URI scheme, which cannot stream).
static LAST_PROGRESS: Mutex<Option<serde_json::Value>> = Mutex::new(None);

/// True while a run is in flight (set by the start handler, cleared by [`RunGuard`] even on panic).
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Cancel flag of the current run.
static CANCEL: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

struct RunGuard;
impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Ask the running index job (if any) to stop at the next batch boundary. Also callable from the desktop
/// shell on quit. Returns whether a run was signalled.
pub fn request_cancel() -> bool {
    if !is_running() {
        return false;
    }
    match CANCEL.lock().unwrap().as_ref() {
        Some(c) => {
            c.store(true, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

fn snapshot(p: &IndexProgress) -> serde_json::Value {
    serde_json::to_value(p).unwrap_or_else(|_| serde_json::json!({ "done": p.done, "total": p.total, "status": p.status }))
}

fn set_last(v: serde_json::Value) {
    *LAST_PROGRESS.lock().unwrap() = Some(v);
}

async fn progress_status() -> Json<serde_json::Value> {
    let v = LAST_PROGRESS.lock().unwrap().clone();
    Json(v.unwrap_or_else(|| serde_json::json!({ "done": 0, "total": 0, "status": "idle", "phase": "idle", "can_cancel": false })))
}

fn progress_broadcast(
) -> &'static Arc<Mutex<Option<tokio::sync::broadcast::Sender<IndexProgress>>>> {
    PROGRESS_TX.get_or_init(|| Arc::new(Mutex::new(None)))
}

async fn start_index(
    State(state): State<Arc<AppState>>,
    Json(body): Json<StartBody>,
) -> Response {
    // Roots must be absolute; store the canonical spelling (an already-registered path keeps its stored form).
    let mut folders: Vec<FolderConfig> = Vec::new();
    {
        let conn = state.pool.get();
        for f in body.folders {
            match crate::api::library::normalize_root_input(&conn, &f.path) {
                Ok(path) => folders.push(FolderConfig { path, source: f.source }),
                Err(msg) => {
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "invalid_folder", "message": msg }))).into_response();
                }
            }
        }
    }


    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "already_running", "message": "Indexing is already running" })),
        )
            .into_response();
    }
    let guard = RunGuard;
    let cancel = Arc::new(AtomicBool::new(false));
    *CANCEL.lock().unwrap() = Some(cancel.clone());

    let (tx, _rx) = tokio::sync::broadcast::channel::<IndexProgress>(256);
    set_last(serde_json::json!({
        "done": 0, "total": 0, "status": "indexing", "phase": "scanning", "total_known": false,
        "discovered": 0, "can_cancel": true, "errors": 0, "skipped": 0, "recent_errors": [],
        "roots_total": folders.len(), "roots_done": 0,
    }));
    {
        let mut guard = progress_broadcast().lock().unwrap();
        *guard = Some(tx.clone());
    }

    let pool = state.pool.clone();
    let indexed_roots = state.indexed_roots.clone();

    tokio::task::spawn_blocking(move || {
        let _guard = guard; // released when the run ends (or panics)
        // Bridge sync SyncSender → broadcast channel + polling snapshot
        let (sync_tx, sync_rx) = mpsc::sync_channel::<IndexProgress>(256);
        let bridge = std::thread::spawn(move || {
            for prog in sync_rx {
                set_last(snapshot(&prog));
                let _ = tx.send(prog);
            }
        });
        // In-memory indexed_roots only learns about roots the run actually registered in the DB.
        if let Ok(outcome) = run_index(folders, pool, sync_tx, cancel) {
            let mut roots = indexed_roots.lock().unwrap();
            for r in outcome.registered_roots {
                if !roots.contains(&r) {
                    roots.push(r);
                }
            }
        }
        // run_index consumed sync_tx; wait for the bridge so the final snapshot is published
        // before RUNNING flips back to false.
        let _ = bridge.join();
    });

    (StatusCode::OK, Json(StartResponse { status: "started".to_string() })).into_response()
}

async fn cancel_index() -> Response {
    if request_cancel() {
        (StatusCode::ACCEPTED, Json(serde_json::json!({ "status": "cancelling" }))).into_response()
    } else {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "not_running", "message": "No indexing run is in progress" })),
        )
            .into_response()
    }
}

async fn progress_sse(
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    // Subscribe first, then replay the latest snapshot, so a late subscriber misses nothing.
    let rx = {
        let guard = progress_broadcast().lock().unwrap();
        guard.as_ref().map(|tx| tx.subscribe())
    };
    let last = LAST_PROGRESS.lock().unwrap().clone();

    let stream = async_stream::stream! {
        let mut finished = false;
        if let Some(v) = last {
            finished = is_terminal_json(&v);
            yield Ok(Event::default().data(v.to_string()));
        }
        if let (false, Some(mut rx)) = (finished, rx) {
            loop {
                match rx.recv().await {
                    Ok(prog) => {
                        let terminal = prog.is_terminal();
                        yield Ok(Event::default().data(snapshot(&prog).to_string()));
                        if terminal { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn is_terminal_json(v: &serde_json::Value) -> bool {
    let s = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    s == "done" || s == "cancelled" || s.starts_with("error")
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/index/start", post(start_index))
        .route("/api/index/progress", get(progress_sse))
        .route("/api/index/status", get(progress_status))
        .route("/api/index/cancel", post(cancel_index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    // Uses the process-global run state, so everything lives in one sequential test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_conflict_cancel_and_status() {
        let dir = std::path::PathBuf::from(format!("/tmp/daouchi-work/index/api-{}", std::process::id()));
        let root = dir.join("lib");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..400 {
            std::fs::write(root.join(format!("b{i}.wav")), b"junk").unwrap();
        }
        let pool = crate::db::open(&dir.join("t.db")).unwrap();
        crate::db_migrate::init_and_migrate(&pool.get()).unwrap();
        let home = crate::lifecycle::init().unwrap().clone();
        let ai = crate::engines::AiEngines::new(home, pool.clone());
        let state = Arc::new(AppState { pool, ai, indexed_roots: Arc::new(Mutex::new(vec![])) });
        let app = router().with_state(state);
        let call = |m: &'static str, u: &'static str, b: Option<String>| {
            let app = app.clone();
            async move {
                let mut r = Request::builder().method(m).uri(u);
                let body = match b {
                    Some(s) => { r = r.header("content-type", "application/json"); Body::from(s) }
                    None => Body::empty(),
                };
                let resp = app.oneshot(r.body(body).unwrap()).await.unwrap();
                let st = resp.status().as_u16();
                let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
                (st, serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default())
            }
        };
        // cancel while idle -> 409
        assert_eq!(call("POST", "/api/index/cancel", None).await.0, 409);
        let body = serde_json::json!({ "folders": [{ "path": root.to_string_lossy(), "source": "sfx" }] }).to_string();
        assert_eq!(call("POST", "/api/index/start", Some(body.clone())).await.0, 200);
        // second concurrent start -> 409 (the run is still going: RUNNING is held until the job ends)
        let (st, j) = call("POST", "/api/index/start", Some(body.clone())).await;
        if is_running() {
            assert_eq!(st, 409);
            assert_eq!(j["error"], "already_running");
        }
        let (st, _) = call("POST", "/api/index/cancel", None).await;
        assert!(st == 202 || st == 409);
        for _ in 0..100 {
            if !is_running() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(!is_running());
        let (st, j) = call("GET", "/api/index/status", None).await;
        assert_eq!(st, 200);
        let status = j["status"].as_str().unwrap();
        assert!(status == "done" || status == "cancelled", "status {status}");
        assert_eq!(j["can_cancel"], false);
        // can start again after finishing
        assert_eq!(call("POST", "/api/index/start", Some(body)).await.0, 200);
        for _ in 0..200 {
            if !is_running() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}
