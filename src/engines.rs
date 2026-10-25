//! In-process AI engines (CLAP, AST, UMAP) and model management.
//!
//! In-process AI engines for the HTTP handlers. Rules:
//! * the DbPool mutex guard is never held across inference or an `.await`;
//! * inference runs in `tokio::task::spawn_blocking`;
//! * a missing model yields `AiError::ModelNotReady` (HTTP 409), never a hang;
//! * background jobs (radar generate / umap) have observable, persisted state and always end in
//!   `done` or `error`.
use crate::db::DbPool;
use anyhow::{anyhow, Result};
use daouchi_ai::{
    ast::{AstEngine, ClapScorer, SuggestedTag},
    audio,
    clap::{self, ClapEngine, EMBED_DIM},
    home::DaouchiHome,
    models::{ModelManager, ModelStatus},
    runtime, umap,
};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex};

/// Value stored in `embeddings.model` for vectors produced by the in-process CLAP engine.
pub const CURRENT_MODEL: &str = "xenova-clap-htsat-unfused";
/// Model tag for embeddings written by an earlier model version (ignored by every query).
#[allow(dead_code)]
pub const LEGACY_MODEL: &str = "laion-630k-legacy";

pub const MODEL_NAMES: [&str; 2] = ["clap", "ast"];
const AST_SAMPLE_RATE: u32 = 16_000;
const EMBED_CHUNK: usize = 32;

#[derive(Debug)]
pub enum AiError {
    ModelNotReady(&'static str),
    /// Engine could not be started (e.g. ONNX Runtime library missing).
    Unavailable(String),
    BadRequest(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiError::ModelNotReady(m) => write!(f, "model '{m}' is not ready"),
            AiError::Unavailable(s) | AiError::BadRequest(s) => write!(f, "{s}"),
            AiError::Other(e) => write!(f, "{e:#}"),
        }
    }
}
impl From<anyhow::Error> for AiError {
    fn from(e: anyhow::Error) -> Self {
        AiError::Other(e)
    }
}
impl From<tokio::task::JoinError> for AiError {
    fn from(e: tokio::task::JoinError) -> Self {
        AiError::Other(anyhow!("task failed: {e}"))
    }
}

#[derive(Debug, Clone)]
pub struct SemanticResult {
    pub sound_id: i64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadInfo {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub file: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub name: &'static str,
    pub label: &'static str,
    pub total_bytes: u64,
    /// "missing" | "partial" | "ready"
    pub status: &'static str,
    pub downloading: Option<DownloadInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// State of the radar background job (embed everything, then UMAP).
#[derive(Debug, Clone, Serialize)]
pub struct RadarJob {
    /// "idle" | "running" | "done" | "error"
    pub state: String,
    /// "" | "embedding" | "umap"
    pub phase: String,
    pub done: u64,
    pub total: u64,
    pub failed: u64,
    /// 0..1 within the current phase
    pub progress: f32,
    pub error: Option<String>,
}

impl RadarJob {
    fn idle() -> Self {
        Self { state: "idle".into(), phase: String::new(), done: 0, total: 0, failed: 0, progress: 0.0, error: None }
    }
}

pub struct AiEngines {
    pub home: DaouchiHome,
    pool: DbPool,
    mgr: Arc<ModelManager>,
    clap: Mutex<Option<Arc<ClapEngine>>>,
    ast: Mutex<Option<Arc<AstEngine>>>,
    tag_text_cache: Arc<StdMutex<HashMap<String, Vec<f32>>>>,
    ready: StdMutex<HashSet<&'static str>>,
    status_cache: StdMutex<HashMap<&'static str, (ModelStatus, Instant)>>,
    downloads: StdMutex<HashMap<&'static str, DownloadInfo>>,
    model_errors: StdMutex<HashMap<&'static str, String>>,
    model_tx: broadcast::Sender<()>,
    job: StdMutex<RadarJob>,
}

fn static_name(name: &str) -> Option<&'static str> {
    MODEL_NAMES.iter().copied().find(|n| *n == name)
}

fn label(name: &str) -> &'static str {
    match name {
        "clap" => "CLAP audio-text embeddings (semantic search, mood, radar)",
        _ => "AST audio tagging (auto-tag suggestions)",
    }
}

impl AiEngines {
    pub fn new(home: DaouchiHome, pool: DbPool) -> Arc<Self> {
        if let Err(e) = runtime::init() {
            tracing::warn!("ONNX Runtime unavailable, AI features disabled until it is installed: {e:#}");
        }
        let (model_tx, _) = broadcast::channel(64);
        let this = Arc::new(Self {
            mgr: Arc::new(ModelManager::new(home.clone())),
            home,
            pool,
            clap: Mutex::new(None),
            ast: Mutex::new(None),
            tag_text_cache: Arc::new(StdMutex::new(HashMap::new())),
            ready: StdMutex::new(HashSet::new()),
            status_cache: StdMutex::new(HashMap::new()),
            downloads: StdMutex::new(HashMap::new()),
            model_errors: StdMutex::new(HashMap::new()),
            model_tx,
            job: StdMutex::new(RadarJob::idle()),
        });
        this.recover_job();
        this
    }

    // ───────────────────────── models ─────────────────────────

    /// Verified status. `Ready` is sticky; other results are cached for a few seconds because
    /// verification hashes every model file.
    async fn status_of(&self, name: &'static str) -> ModelStatus {
        if self.ready.lock().unwrap().contains(name) {
            return ModelStatus::Ready;
        }
        if let Some((st, at)) = self.status_cache.lock().unwrap().get(name) {
            if at.elapsed() < Duration::from_secs(5) {
                return *st;
            }
        }
        let mgr = self.mgr.clone();
        let st = tokio::task::spawn_blocking(move || mgr.status(name)).await;
        let st = match st {
            Ok(Ok(s)) => s,
            _ => ModelStatus::Missing,
        };
        if st == ModelStatus::Ready {
            self.ready.lock().unwrap().insert(name);
        }
        self.status_cache.lock().unwrap().insert(name, (st, Instant::now()));
        st
    }

    pub async fn model_status(&self) -> Vec<ModelInfo> {
        let mut out = Vec::new();
        for name in MODEL_NAMES {
            let total_bytes = self.mgr.manifest(name).map(|m| m.files.iter().map(|f| f.size).sum()).unwrap_or(0);
            let status = match self.status_of(name).await {
                ModelStatus::Ready => "ready",
                ModelStatus::Partial => "partial",
                ModelStatus::Missing => "missing",
            };
            out.push(ModelInfo {
                name,
                label: label(name),
                total_bytes,
                status,
                downloading: self.downloads.lock().unwrap().get(name).cloned(),
                error: self.model_errors.lock().unwrap().get(name).cloned(),
            });
        }
        out
    }

    pub fn subscribe_models(&self) -> broadcast::Receiver<()> {
        self.model_tx.subscribe()
    }

    fn notify_models(&self) {
        let _ = self.model_tx.send(());
    }

    /// Start a background download/verify of `name` (idempotent). Returns the current status list.
    pub async fn ensure_model(self: &Arc<Self>, name: &str) -> Result<Vec<ModelInfo>, AiError> {
        let name = static_name(name).ok_or_else(|| AiError::BadRequest(format!("unknown model '{name}'")))?;
        let already = {
            let mut d = self.downloads.lock().unwrap();
            if d.contains_key(name) {
                true
            } else {
                d.insert(name, DownloadInfo { done_bytes: 0, total_bytes: 0, file: String::new() });
                false
            }
        };
        if !already && self.status_of(name).await != ModelStatus::Ready {
            self.model_errors.lock().unwrap().remove(name);
            let this = self.clone();
            tokio::spawn(async move {
                let last = StdMutex::new(Instant::now() - Duration::from_secs(1));
                let t2 = this.clone();
                let res = this
                    .mgr
                    .ensure(name, move |p| {
                        t2.downloads.lock().unwrap().insert(
                            name,
                            DownloadInfo { done_bytes: p.bytes_done, total_bytes: p.bytes_total, file: p.file.clone() },
                        );
                        let mut l = last.lock().unwrap();
                        if l.elapsed() >= Duration::from_millis(250) {
                            *l = Instant::now();
                            t2.notify_models();
                        }
                    })
                    .await;
                if let Err(e) = res {
                    tracing::warn!("model {name} download failed: {e:#}");
                    this.model_errors.lock().unwrap().insert(name, format!("{e:#}"));
                }
                this.downloads.lock().unwrap().remove(name);
                this.status_cache.lock().unwrap().remove(name);
                this.notify_models();
            });
        } else if !already {
            self.downloads.lock().unwrap().remove(name);
        }
        self.notify_models();
        Ok(self.model_status().await)
    }

    async fn clap(&self) -> Result<Arc<ClapEngine>, AiError> {
        let mut g = self.clap.lock().await;
        if let Some(e) = &*g {
            return Ok(e.clone());
        }
        if self.status_of("clap").await != ModelStatus::Ready {
            return Err(AiError::ModelNotReady("clap"));
        }
        let dir = self.home.model_dir("clap");
        let t = Instant::now();
        let e = tokio::task::spawn_blocking(move || ClapEngine::load(&dir))
            .await?
            .map_err(|e| AiError::Unavailable(format!("cannot start CLAP engine: {e:#}")))?;
        tracing::info!("CLAP engine loaded in {:?}", t.elapsed());
        let e = Arc::new(e);
        *g = Some(e.clone());
        Ok(e)
    }

    async fn ast(&self) -> Result<Arc<AstEngine>, AiError> {
        let mut g = self.ast.lock().await;
        if let Some(e) = &*g {
            return Ok(e.clone());
        }
        if self.status_of("ast").await != ModelStatus::Ready {
            return Err(AiError::ModelNotReady("ast"));
        }
        let path = self.mgr.file_path("ast", "onnx/model.onnx");
        let t = Instant::now();
        let e = tokio::task::spawn_blocking(move || {
            runtime::init()?;
            AstEngine::load_file(&path, &runtime::SessionOpts::default())
        })
        .await?
        .map_err(|e| AiError::Unavailable(format!("cannot start AST engine: {e:#}")))?;
        tracing::info!("AST engine loaded in {:?}", t.elapsed());
        let e = Arc::new(e);
        *g = Some(e.clone());
        Ok(e)
    }

    // ───────────────────────── DB helpers ─────────────────────────

    fn load_embeddings(pool: &DbPool) -> Result<Vec<(i64, Vec<f32>)>> {
        let conn = pool.get();
        let mut stmt = conn.prepare("SELECT sound_id, vector FROM embeddings WHERE model = ?1")?;
        let rows = stmt.query_map([CURRENT_MODEL], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        Ok(rows.flatten().filter_map(|(id, b)| clap::bytes_to_vec(&b).map(|v| (id, v))).collect())
    }

    fn store_embedding(pool: &DbPool, sound_id: i64, v: &[f32]) -> Result<()> {
        let conn = pool.get();
        conn.execute(
            "INSERT OR REPLACE INTO embeddings (sound_id, vector, model) VALUES (?1, ?2, ?3)",
            rusqlite::params![sound_id, clap::vec_to_bytes(v), CURRENT_MODEL],
        )?;
        Ok(())
    }

    // ───────────────────────── operations ─────────────────────────

    pub async fn semantic_search(&self, query: &str, limit: usize) -> Result<Vec<SemanticResult>, AiError> {
        let clap = self.clap().await?;
        let q = query.to_string();
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<SemanticResult>> {
            let qv = clap.embed_text(&q)?;
            let rows = Self::load_embeddings(&pool)?;
            let it = rows.iter().map(|(i, v)| (*i, v.as_slice()));
            Ok(clap::rank(&qv, it, limit)
                .into_iter()
                .map(|(sound_id, s)| SemanticResult { sound_id, score: s as f64 })
                .collect())
        })
        .await?
        .map_err(Into::into)
    }

    #[allow(dead_code)]
    /// Embed `items` (sound_id, path) and store them. Returns (processed, failed).
    pub async fn embed_batch(&self, items: Vec<(i64, String)>) -> Result<(usize, usize), AiError> {
        let clap = self.clap().await?;
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || Ok(embed_chunk(&clap, &pool, &items).map(|(ok, fail, _)| (ok, fail))?))
            .await?
    }

    pub async fn sfx_category_counts(&self) -> Result<serde_json::Value, AiError> {
        let pool = self.pool.clone();
        let rows = tokio::task::spawn_blocking(move || Self::load_embeddings(&pool)).await??;
        let total = rows.len();
        let mut counts = serde_json::Map::new();
        if total == 0 {
            for (cat, _) in clap::SFX_CATEGORY_QUERIES {
                counts.insert(cat.into(), 0.into());
            }
        } else {
            let clap = self.clap().await?;
            let res = tokio::task::spawn_blocking(move || {
                let it = rows.iter().map(|(i, v)| (*i, v.as_slice()));
                clap.sfx_category_counts(it)
            })
            .await?;
            for (cat, n) in res {
                counts.insert(cat.into(), n.into());
            }
        }
        Ok(serde_json::json!({ "counts": counts, "total_embedded": total }))
    }

    /// Run AST tagging (with CLAP cross-validation when CLAP is ready) and store suggestions.
    pub async fn autotag(&self, sound_id: i64, path: &str) -> Result<Vec<SuggestedTag>, AiError> {
        let ast = self.ast().await?;
        // CLAP is optional here: without it, tags are not cross-validated.
        let clap = match self.clap().await {
            Ok(c) => Some(c),
            Err(AiError::ModelNotReady(_)) => None,
            Err(e) => {
                tracing::warn!("autotag without CLAP cross-validation: {e}");
                None
            }
        };
        let pool = self.pool.clone();
        let cache = self.tag_text_cache.clone();
        let path = path.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<SuggestedTag>> {
            let samples = audio::load_mono(Path::new(&path), AST_SAMPLE_RATE)?;
            let stored: Option<Vec<f32>> = {
                let conn = pool.get();
                conn.query_row(
                    "SELECT vector FROM embeddings WHERE sound_id = ?1 AND model = ?2",
                    rusqlite::params![sound_id, CURRENT_MODEL],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .ok()
                .and_then(|b| clap::bytes_to_vec(&b))
            };
            let emb = match (&stored, &clap) {
                (Some(v), _) => Some(v.clone()),
                (None, Some(c)) => match c.embed_audio_file(Path::new(&path)) {
                    Ok(v) => {
                        let _ = Self::store_embedding(&pool, sound_id, &v);
                        Some(v.to_vec())
                    }
                    Err(e) => {
                        tracing::warn!("autotag: CLAP embedding failed for {path}: {e:#}");
                        None
                    }
                },
                _ => None,
            };
            let scorer = clap.as_ref().map(|c| Scorer { clap: c.as_ref(), cache: &cache });
            let (emb_ref, scorer_ref) = match (&emb, &scorer) {
                (Some(e), Some(s)) => (Some(e.as_slice()), Some(s as &dyn ClapScorer)),
                _ => (None, None),
            };
            let tags = ast.tag(&samples, Some(&path), emb_ref, scorer_ref)?;
            let conn = pool.get();
            for t in &tags {
                conn.execute(
                    "INSERT OR REPLACE INTO suggested_tags (sound_id, tag, score) VALUES (?1, ?2, ?3)",
                    rusqlite::params![sound_id, t.tag, t.score as f64],
                )?;
            }
            Ok(tags)
        })
        .await?
        .map_err(Into::into)
    }

    // ───────────────────────── radar job ─────────────────────────

    pub fn job(&self) -> RadarJob {
        self.job.lock().unwrap().clone()
    }

    fn set_job(&self, f: impl FnOnce(&mut RadarJob)) {
        let snap = {
            let mut j = self.job.lock().unwrap();
            f(&mut j);
            j.clone()
        };
        self.persist_job(&snap);
    }

    /// Update in-memory progress only (no DB write).
    fn tick_job(&self, f: impl FnOnce(&mut RadarJob)) {
        f(&mut self.job.lock().unwrap());
    }

    fn persist_job(&self, j: &RadarJob) {
        let conn = self.pool.get();
        let r = conn.execute(
            "INSERT OR REPLACE INTO ai_jobs (name, state, phase, done, total, failed, error, updated_at) \
             VALUES ('radar', ?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            rusqlite::params![j.state, j.phase, j.done as i64, j.total as i64, j.failed as i64, j.error],
        );
        if let Err(e) = r {
            tracing::warn!("persist job state: {e}");
        }
    }

    /// On startup: load the last persisted state; a job that was "running" died with the process.
    fn recover_job(&self) {
        let row = {
            let conn = self.pool.get();
            conn.query_row(
                "SELECT state, phase, done, total, failed, error FROM ai_jobs WHERE name = 'radar'",
                [],
                |r| {
                    Ok(RadarJob {
                        state: r.get(0)?,
                        phase: r.get(1)?,
                        done: r.get::<_, i64>(2)? as u64,
                        total: r.get::<_, i64>(3)? as u64,
                        failed: r.get::<_, i64>(4)? as u64,
                        progress: 0.0,
                        error: r.get(5)?,
                    })
                },
            )
            .ok()
        };
        if let Some(mut j) = row {
            if j.state == "running" {
                j.state = "error".into();
                j.error = Some("interrupted: the app was closed while the job was running".into());
            }
            self.set_job(|cur| *cur = j);
        }
    }

    /// Embed every sound lacking a current-model embedding, then run UMAP. Returns immediately.
    pub async fn start_radar_generate(self: &Arc<Self>) -> Result<RadarJob, AiError> {
        let pool = self.pool.clone();
        let todo = tokio::task::spawn_blocking(move || -> Result<Vec<(i64, String)>> {
            let conn = pool.get();
            let mut stmt = conn.prepare(
                "SELECT s.id, s.path FROM sounds s LEFT JOIN embeddings e ON e.sound_id = s.id \
                 WHERE e.sound_id IS NULL OR e.model != ?1 ORDER BY s.id",
            )?;
            let rows = stmt.query_map([CURRENT_MODEL], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            Ok(rows.flatten().collect())
        })
        .await??;
        if !todo.is_empty() {
            // fail fast (409) instead of starting a job that cannot work
            self.clap().await?;
        }
        self.begin_job(todo, true)
    }

    /// UMAP only, over existing current-model embeddings.
    pub async fn start_umap(self: &Arc<Self>) -> Result<RadarJob, AiError> {
        let pool = self.pool.clone();
        let n = tokio::task::spawn_blocking(move || {
            pool.get()
                .query_row("SELECT COUNT(*) FROM embeddings WHERE model = ?1", [CURRENT_MODEL], |r| r.get::<_, i64>(0))
        })
        .await?
        .map_err(|e| AiError::Other(e.into()))?;
        if n == 0 {
            return Err(AiError::BadRequest("no embeddings from the current model; run radar/generate first".into()));
        }
        self.begin_job(Vec::new(), false)
    }

    fn begin_job(self: &Arc<Self>, todo: Vec<(i64, String)>, _embed: bool) -> Result<RadarJob, AiError> {
        {
            let j = self.job.lock().unwrap();
            if j.state == "running" {
                return Ok(j.clone());
            }
        }
        let total = todo.len() as u64;
        self.set_job(|j| {
            *j = RadarJob {
                state: "running".into(),
                phase: if total > 0 { "embedding".into() } else { "umap".into() },
                done: 0,
                total,
                failed: 0,
                progress: 0.0,
                error: None,
            }
        });
        let this = self.clone();
        tokio::spawn(async move {
            match this.run_job(todo).await {
                Ok(()) => this.set_job(|j| {
                    j.state = "done".into();
                    j.phase.clear();
                    j.progress = 1.0;
                }),
                Err(e) => {
                    tracing::warn!("radar job failed: {e}");
                    this.set_job(|j| {
                        j.state = "error".into();
                        j.error = Some(e.to_string());
                    });
                }
            }
        });
        Ok(self.job())
    }

    async fn run_job(self: &Arc<Self>, todo: Vec<(i64, String)>) -> Result<(), AiError> {
        if !todo.is_empty() {
            let clap = self.clap().await?;
            let mut first_err: Option<String> = None;
            let t = Instant::now();
            for chunk in todo.chunks(EMBED_CHUNK) {
                let (c, p, items) = (clap.clone(), self.pool.clone(), chunk.to_vec());
                let (ok, fail, err) = tokio::task::spawn_blocking(move || embed_chunk(&c, &p, &items)).await??;
                if first_err.is_none() {
                    first_err = err;
                }
                self.tick_job(|j| {
                    j.done += (ok + fail) as u64;
                    j.failed += fail as u64;
                    j.progress = j.done as f32 / j.total.max(1) as f32;
                });
            }
            tracing::info!("embedded {} files in {:?}", todo.len(), t.elapsed());
            let pool = self.pool.clone();
            let have = tokio::task::spawn_blocking(move || {
                pool.get().query_row("SELECT COUNT(*) FROM embeddings WHERE model = ?1", [CURRENT_MODEL], |r| r.get::<_, i64>(0))
            })
            .await?
            .map_err(|e| AiError::Other(e.into()))?;
            if have == 0 {
                return Err(AiError::Other(anyhow!(
                    "no files could be embedded ({} failed){}",
                    todo.len(),
                    first_err.map(|e| format!(": {e}")).unwrap_or_default()
                )));
            }
            self.set_job(|j| {
                j.phase = "umap".into();
                j.progress = 0.0;
            });
        }
        let (this, pool) = (self.clone(), self.pool.clone());
        let t = Instant::now();
        let n = tokio::task::spawn_blocking(move || -> Result<usize> {
            let rows = Self::load_embeddings(&pool)?;
            let (ids, vecs): (Vec<i64>, Vec<Vec<f32>>) = rows.into_iter().unzip();
            let pts = umap::project_2d(&ids, &vecs, &umap::UmapParams::default(), |p| {
                this.tick_job(|j| j.progress = p)
            })?;
            let mut conn = pool.get();
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM radar_coords", [])?;
            {
                let mut ins = tx.prepare("INSERT INTO radar_coords (sound_id, x, y) VALUES (?1, ?2, ?3)")?;
                for (id, x, y) in &pts {
                    ins.execute(rusqlite::params![id, *x as f64, *y as f64])?;
                }
            }
            tx.commit()?;
            Ok(pts.len())
        })
        .await??;
        tracing::info!("UMAP over {n} points in {:?}", t.elapsed());
        Ok(())
    }
}

/// Embed + store a chunk. Per-file failures are counted, not fatal. Returns (ok, failed, first error).
fn embed_chunk(clap: &ClapEngine, pool: &DbPool, items: &[(i64, String)]) -> Result<(usize, usize, Option<String>)> {
    // inference without any DB lock held
    let results: Vec<(i64, std::result::Result<[f32; EMBED_DIM], String>)> = items
        .par_iter()
        .map(|(id, p)| (*id, clap.embed_audio_file(Path::new(p)).map_err(|e| format!("{p}: {e:#}"))))
        .collect();
    let (mut ok, mut fail, mut first) = (0, 0, None);
    let conn = pool.get();
    for (id, r) in results {
        match r {
            Ok(v) => {
                conn.execute(
                    "INSERT OR REPLACE INTO embeddings (sound_id, vector, model) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, clap::vec_to_bytes(&v), CURRENT_MODEL],
                )?;
                ok += 1;
            }
            Err(e) => {
                tracing::warn!("embed failed: {e}");
                fail += 1;
                first.get_or_insert(e);
            }
        }
    }
    Ok((ok, fail, first))
}

struct Scorer<'a> {
    clap: &'a ClapEngine,
    cache: &'a StdMutex<HashMap<String, Vec<f32>>>,
}

impl ClapScorer for Scorer<'_> {
    fn tag_similarity(&self, audio_embedding: &[f32], tag: &str) -> Result<f32> {
        let cached = self.cache.lock().unwrap().get(tag).cloned();
        let t = match cached {
            Some(t) => t,
            None => {
                let v = self.clap.embed_text(tag)?.to_vec();
                self.cache.lock().unwrap().insert(tag.to_string(), v.clone());
                v
            }
        };
        let dot: f32 = audio_embedding.iter().zip(&t).map(|(a, b)| a * b).sum();
        let na = audio_embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nt = t.iter().map(|x| x * x).sum::<f32>().sqrt();
        Ok(dot / (na * nt + 1e-8))
    }
}
