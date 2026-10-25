use crate::db::DbPool;
use anyhow::Result;
use rayon::prelude::*;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct FolderConfig {
    pub path: String,
    pub source: String,
}

/// One problem file: `path` is relative to its root (truncated), `reason` is short plain text.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IndexIssue {
    pub path: String,
    pub reason: String,
}

/// The single progress snapshot shared by SSE (`/api/index/progress`) and polling (`/api/index/status`).
/// `done`, `total` and `status` are the legacy fields and keep their old meaning.
#[derive(Debug, Clone, Serialize)]
pub struct IndexProgress {
    /// scanning | reading | writing | finalizing | done | cancelled | error
    pub phase: String,
    /// Legacy: "indexing" | "done" | "cancelled" | "error: <msg>".
    pub status: String,
    /// Files fully processed (read + saved, or skipped).
    pub done: usize,
    /// Files to process; 0 until the scan finishes (`total_known`).
    pub total: usize,
    pub total_known: bool,
    /// Audio files found so far by the scan.
    pub discovered: usize,
    /// Rows actually written to the library (new or refreshed).
    pub added: usize,
    pub current_root: Option<String>,
    pub current_path: Option<String>,
    pub roots_done: usize,
    pub roots_total: usize,
    /// Files that could not be opened/read at all and were NOT added.
    pub skipped: usize,
    /// All problem files (skipped ones + files added without audio details + unreadable folders).
    pub errors: usize,
    /// The last (max 20) problems.
    pub recent_errors: Vec<IndexIssue>,
    pub bytes_done: u64,
    pub started_at_ms: u64,
    pub elapsed_ms: u64,
    pub files_per_sec: f64,
    pub eta_ms: Option<u64>,
    pub can_cancel: bool,
    /// Set for phase "error".
    pub message: Option<String>,
}

impl IndexProgress {
    pub fn is_terminal(&self) -> bool {
        matches!(self.phase.as_str(), "done" | "cancelled" | "error")
    }
}

/// Result of a run.
#[derive(Debug, Clone)]
pub struct IndexOutcome {
    pub done: usize,
    pub added: usize,
    pub cancelled: bool,
    /// Roots now registered in `indexed_roots` (all of them for a full run; only touched ones if cancelled).
    pub registered_roots: Vec<String>,
}

const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "m4a", "flac", "aiff", "aif"];
/// Files read+written per batch; cancel is honoured at batch boundaries.
const BATCH: usize = 100;
const MAX_ERRORS: usize = 20;
const EMIT_EVERY: Duration = Duration::from_millis(120);

struct AudioMeta {
    path: PathBuf,
    filename: String,
    extension: String,
    size: Option<i64>,
    duration: Option<f64>,
    sample_rate: Option<i64>,
    bitrate: Option<i64>,
    bit_depth: Option<i64>,
    collection: Option<String>,
    folder_path: Option<String>,
    source: String,
    file_modified_at: Option<String>,
    /// Audio header could not be parsed (row is still added, without details).
    soft_error: Option<String>,
}

fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

fn derive_collection_and_folder(
    file_path: &Path,
    root: &Path,
) -> (Option<String>, Option<String>) {
    let rel = match file_path.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return (None, None),
    };

    let component_count = rel.components().count();

    let collection = if component_count > 1 {
        rel.components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
    } else {
        None
    };

    let folder = file_path
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty());

    (collection, folder)
}

type Probe = (Option<f64>, Option<i64>, Option<i64>, Option<i64>);

fn probe_audio(path: &Path) -> Result<Probe, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probe_result = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|_| "unsupported or corrupt audio".to_string())?;
    let format = probe_result.format;
    let track = format.default_track().ok_or_else(|| "no audio track".to_string())?;
    let params = &track.codec_params;

    let sample_rate = params.sample_rate.map(|r| r as i64);
    let bit_depth = params.bits_per_sample.map(|b| b as i64);
    let duration = if let (Some(frames), Some(tb)) = (params.n_frames, params.time_base) {
        Some(frames as f64 * tb.numer as f64 / tb.denom as f64)
    } else {
        None
    };
    // Approximate bitrate from encoded bits/sample x sample_rate x channels
    let bitrate = params
        .bits_per_coded_sample
        .and_then(|bps| {
            params.sample_rate.map(|sr| {
                let ch = params.channels.map(|c| c.count() as u64).unwrap_or(2);
                (bps as u64 * sr as u64 * ch) / 1000
            })
        })
        .map(|b| b as i64);
    Ok((duration, sample_rate, bitrate, bit_depth))
}

fn rel_display(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_string();
    truncate_tail(&rel, 90)
}

fn truncate_tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("\u{2026}{tail}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Builds and emits snapshots; owns speed smoothing and ETA.
struct Tracker {
    tx: SyncSender<IndexProgress>,
    t0: Instant,
    p: IndexProgress,
    last_emit: Option<Instant>,
    ema: f64,
    sample_t: Instant,
    sample_done: usize,
    proc_start: Option<Instant>,
}

impl Tracker {
    fn new(tx: SyncSender<IndexProgress>, roots_total: usize) -> Self {
        let now = Instant::now();
        Tracker {
            tx,
            t0: now,
            p: IndexProgress {
                phase: "scanning".into(),
                status: "indexing".into(),
                done: 0,
                total: 0,
                total_known: false,
                discovered: 0,
                added: 0,
                current_root: None,
                current_path: None,
                roots_done: 0,
                roots_total,
                skipped: 0,
                errors: 0,
                recent_errors: vec![],
                bytes_done: 0,
                started_at_ms: now_ms(),
                elapsed_ms: 0,
                files_per_sec: 0.0,
                eta_ms: None,
                can_cancel: true,
                message: None,
            },
            last_emit: None,
            ema: 0.0,
            sample_t: now,
            sample_done: 0,
            proc_start: None,
        }
    }

    fn issue(&mut self, path: String, reason: String) {
        self.p.errors += 1;
        self.p.recent_errors.push(IndexIssue { path, reason });
        if self.p.recent_errors.len() > MAX_ERRORS {
            self.p.recent_errors.remove(0);
        }
    }

    fn refresh(&mut self) {
        self.p.elapsed_ms = self.t0.elapsed().as_millis() as u64;
        let processing = matches!(self.p.phase.as_str(), "reading" | "writing");
        if processing && self.sample_t.elapsed() >= Duration::from_millis(500) {
            let dt = self.sample_t.elapsed().as_secs_f64();
            let inst = (self.p.done.saturating_sub(self.sample_done)) as f64 / dt;
            self.ema = if self.ema == 0.0 { inst } else { 0.7 * self.ema + 0.3 * inst };
            self.sample_t = Instant::now();
            self.sample_done = self.p.done;
        }
        self.p.files_per_sec = (self.ema * 10.0).round() / 10.0;
        let stable = self.proc_start.map(|t| t.elapsed() >= Duration::from_millis(1500)).unwrap_or(false)
            && self.p.done >= 30;
        self.p.eta_ms = if processing && self.p.total_known && stable && self.ema > 0.0 {
            Some((self.p.total.saturating_sub(self.p.done) as f64 / self.ema * 1000.0) as u64)
        } else {
            None
        };
    }

    fn emit(&mut self, force: bool) {
        if !force && self.last_emit.map(|t| t.elapsed() < EMIT_EVERY).unwrap_or(false) {
            return;
        }
        self.refresh();
        self.last_emit = Some(Instant::now());
        let _ = self.tx.send(self.p.clone());
    }

    fn phase(&mut self, phase: &str) {
        self.p.phase = phase.into();
        if phase == "reading" && self.proc_start.is_none() {
            self.proc_start = Some(Instant::now());
            self.sample_t = Instant::now();
        }
        self.emit(true);
    }

    /// Final snapshot (phase done | cancelled | error).
    fn finish(&mut self, phase: &str, message: Option<String>) {
        self.p.phase = phase.into();
        self.p.status = match phase {
            "error" => format!("error: {}", message.clone().unwrap_or_default()),
            other => other.into(),
        };
        self.p.message = message;
        self.p.can_cancel = false;
        self.p.current_path = None;
        self.p.eta_ms = None;
        self.refresh();
        if let Some(t) = self.proc_start {
            let s = t.elapsed().as_secs_f64();
            if s > 0.0 && self.p.done > 0 {
                self.p.files_per_sec = ((self.p.done as f64 / s) * 10.0).round() / 10.0;
            }
        }
        self.p.eta_ms = None;
        self.last_emit = Some(Instant::now());
        let _ = self.tx.send(self.p.clone());
    }
}

/// Walk all roots. Returns `None` if cancelled. `ends[i]` = cumulative file count after root i.
fn scan(
    folders: &[FolderConfig],
    tr: &mut Tracker,
    cancel: &AtomicBool,
) -> Option<(Vec<(PathBuf, String, PathBuf)>, Vec<usize>)> {
    let mut files = Vec::new();
    let mut ends = Vec::new();
    for (ri, folder) in folders.iter().enumerate() {
        let root = PathBuf::from(&folder.path);
        tr.p.current_root = Some(folder.path.clone());
        let walker = WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !is_hidden(e));
        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    let p = e.path().map(|p| rel_display(p, &root)).unwrap_or_else(|| folder.path.clone());
                    let reason = e
                        .io_error()
                        .map(|io| match io.kind() {
                            std::io::ErrorKind::NotFound => "folder not found".to_string(),
                            std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
                            _ => io.to_string(),
                        })
                        .unwrap_or_else(|| e.to_string());
                    tr.issue(p, reason);
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path().to_path_buf();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                tr.p.discovered += 1;
                tr.p.current_path = Some(rel_display(&path, &root));
                files.push((path, folder.source.clone(), root.clone()));
                tr.emit(false);
            }
        }
        ends.push(files.len());
        tr.p.roots_done = ri + 1;
        tr.emit(true);
    }
    Some((files, ends))
}

fn read_one(path: PathBuf, source: String, root: PathBuf) -> Result<AudioMeta, (PathBuf, String, PathBuf)> {
    let meta_fs = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return Err((path, format!("cannot read file: {e}"), root)),
    };
    if let Err(e) = std::fs::File::open(&path) {
        return Err((path, format!("cannot open: {e}"), root));
    }
    let filename = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let size = Some(meta_fs.len() as i64);
    let file_modified_at = meta_fs.modified().ok().map(|t| {
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
            .unwrap_or_default()
            .to_rfc3339()
    });
    let (probe, soft_error) = match probe_audio(&path) {
        Ok(p) => (p, None),
        Err(e) => ((None, None, None, None), Some(e)),
    };
    let (duration, sample_rate, bitrate, bit_depth) = probe;
    let (collection, folder_path) = derive_collection_and_folder(&path, &root);
    Ok(AudioMeta {
        path, filename, extension, size, duration, sample_rate, bitrate, bit_depth,
        collection, folder_path, source, file_modified_at, soft_error,
    })
}

fn write_batch(pool: &DbPool, metas: &[&AudioMeta], now: &str) -> Result<()> {
    let conn = pool.get();
    conn.execute_batch("BEGIN")?;
    let res = (|| -> Result<()> {
        for meta in metas {
            // ON CONFLICT upsert keeps the row `id`, so tags/favorites/ratings/embeddings (ON DELETE CASCADE
            // children and `favorite`/`rating` columns) are never touched. Never INSERT OR REPLACE here.
            conn.execute(
                r#"INSERT INTO sounds
                    (path, filename, extension, size, duration, bitrate, sample_rate,
                     bit_depth, collection, folder_path, source, favorite, indexed_at, file_modified_at)
                   VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,?12,?13)
                   ON CONFLICT(path) DO UPDATE SET
                    filename         = excluded.filename,
                    extension        = excluded.extension,
                    size             = excluded.size,
                    duration         = excluded.duration,
                    bitrate          = excluded.bitrate,
                    sample_rate      = excluded.sample_rate,
                    bit_depth        = excluded.bit_depth,
                    collection       = excluded.collection,
                    folder_path      = excluded.folder_path,
                    source           = excluded.source,
                    indexed_at       = excluded.indexed_at,
                    file_modified_at = excluded.file_modified_at"#,
                rusqlite::params![
                    meta.path.to_string_lossy().to_string(),
                    meta.filename, meta.extension, meta.size, meta.duration, meta.bitrate,
                    meta.sample_rate, meta.bit_depth, meta.collection, meta.folder_path,
                    meta.source, now, meta.file_modified_at,
                ],
            )?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => conn.execute_batch("COMMIT").map_err(Into::into),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn register_roots(pool: &DbPool, roots: &[&FolderConfig]) -> Result<()> {
    let conn = pool.get();
    conn.execute_batch("BEGIN")?;
    let res = (|| -> Result<()> {
        for f in roots {
            conn.execute(
                "INSERT OR REPLACE INTO indexed_roots (path, source) VALUES (?1, ?2)",
                rusqlite::params![f.path, f.source],
            )?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => conn.execute_batch("COMMIT").map_err(Into::into),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Index `folders` into the library.
///
/// Safety model: purely additive. Rows are upserted by `path` in per-batch transactions; nothing is deleted
/// (not even files that vanished from disk) and user data tables are never touched. Cancelling (`cancel` set),
/// crashing or quitting mid-run therefore leaves the previous library intact plus whatever batches had already
/// been committed. A final snapshot (phase done | cancelled | error) is always sent.
pub fn run_index(
    folders: Vec<FolderConfig>,
    pool: DbPool,
    tx: SyncSender<IndexProgress>,
    cancel: Arc<AtomicBool>,
) -> Result<IndexOutcome> {
    let mut tr = Tracker::new(tx, folders.len());
    match run_inner(&folders, &pool, &mut tr, &cancel) {
        Ok(o) => {
            tr.finish(if o.cancelled { "cancelled" } else { "done" }, None);
            Ok(o)
        }
        Err(e) => {
            tr.finish("error", Some(format!("{e:#}")));
            Err(e)
        }
    }
}

fn run_inner(
    folders: &[FolderConfig],
    pool: &DbPool,
    tr: &mut Tracker,
    cancel: &AtomicBool,
) -> Result<IndexOutcome> {
    tr.emit(true);
    let (files, ends) = match scan(folders, tr, cancel) {
        Some(x) => x,
        None => {
            return Ok(IndexOutcome { done: 0, added: 0, cancelled: true, registered_roots: vec![] });
        }
    };
    tr.p.total = files.len();
    tr.p.total_known = true;
    tr.p.roots_done = 0;

    let now = chrono::Utc::now().to_rfc3339();
    let mut cancelled = false;
    let mut pos = 0usize;
    let roots_done_at = |done: usize| ends.iter().filter(|&&e| e <= done).count();

    for batch in files.chunks(BATCH) {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        tr.p.current_root = batch.first().map(|b| b.2.to_string_lossy().to_string());
        tr.p.current_path = batch.first().map(|b| rel_display(&b.0, &b.2));
        tr.phase("reading");
        let read: Vec<Result<AudioMeta, _>> = batch
            .to_vec()
            .into_par_iter()
            .map(|(p, s, r)| read_one(p, s, r))
            .collect();

        let mut ok: Vec<&AudioMeta> = Vec::new();
        for r in &read {
            match r {
                Ok(m) => ok.push(m),
                Err((p, reason, root)) => {
                    tr.p.skipped += 1;
                    tr.issue(rel_display(p, root), reason.clone());
                }
            }
        }
        tr.phase("writing");
        write_batch(pool, &ok, &now)?;
        for m in &ok {
            if let Some(reason) = &m.soft_error {
                let root = batch.iter().find(|b| b.0 == m.path).map(|b| b.2.clone()).unwrap_or_default();
                tr.issue(rel_display(&m.path, &root), format!("added without audio details: {reason}"));
            }
            tr.p.bytes_done += m.size.unwrap_or(0).max(0) as u64;
        }
        tr.p.added += ok.len();
        pos += batch.len();
        tr.p.done = pos;
        tr.p.roots_done = roots_done_at(pos);
        if let Some(last) = batch.last() {
            tr.p.current_path = Some(rel_display(&last.0, &last.2));
        }
        tr.emit(false);
    }

    // Finalizing: register roots (needed by the audio path-security check) in one transaction. Cancelled runs
    // only register roots from which at least one file was processed.
    tr.p.can_cancel = false;
    tr.p.current_path = None;
    tr.phase("finalizing");
    let mut start = 0usize;
    let mut reg: Vec<&FolderConfig> = Vec::new();
    for (i, f) in folders.iter().enumerate() {
        if !cancelled || start < pos {
            reg.push(f);
        }
        start = ends[i];
    }
    register_roots(pool, &reg)?;
    Ok(IndexOutcome {
        done: pos,
        added: tr.p.added,
        cancelled,
        registered_roots: reg.iter().map(|f| f.path.clone()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn tmp(name: &str) -> PathBuf {
        let d = PathBuf::from(format!("/tmp/daouchi-work/index/{}-{}-{}", name, std::process::id(), now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn wav(path: &Path, frames: u32) {
        let data = frames * 2;
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend((36 + data).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(8000u32.to_le_bytes());
        b.extend(16000u32.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend(data.to_le_bytes());
        b.extend(vec![0u8; data as usize]);
        std::fs::write(path, b).unwrap();
    }

    fn db(dir: &Path) -> DbPool {
        let pool = crate::db::open(&dir.join("t.db")).unwrap();
        crate::db_migrate::init_and_migrate(&pool.get()).unwrap();
        pool
    }

    fn tree(root: &Path, n: usize) {
        for i in 0..n {
            let sub = root.join(format!("pack{}", i % 3));
            std::fs::create_dir_all(&sub).unwrap();
            wav(&sub.join(format!("s{i:04}.wav")), 800);
        }
    }

    fn run(
        folders: Vec<FolderConfig>,
        pool: &DbPool,
        cancel: Arc<AtomicBool>,
        cancel_when: impl Fn(&IndexProgress) -> bool + Send + 'static,
    ) -> (Result<IndexOutcome>, Vec<IndexProgress>) {
        let (tx, rx) = sync_channel(1024);
        let p = pool.clone();
        let c = cancel.clone();
        let h = std::thread::spawn(move || run_index(folders, p, tx, c));
        let mut snaps = vec![];
        for s in rx {
            if cancel_when(&s) {
                cancel.store(true, Ordering::SeqCst);
            }
            snaps.push(s);
        }
        (h.join().unwrap(), snaps)
    }

    /// Add a tag the way api/metadata.rs does (tags row + manual FTS resync).
    fn add_tag(pool: &DbPool, id: i64, tag: &str) {
        let c = pool.get();
        let (f, fp, col): (String, Option<String>, Option<String>) = c
            .query_row("SELECT filename, folder_path, collection FROM sounds WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        c.execute("INSERT INTO tags (sound_id, tag) VALUES (?,?)", rusqlite::params![id, tag]).unwrap();
        c.execute("INSERT INTO sounds_fts(sounds_fts, rowid, filename, folder_path, collection, tags) VALUES('delete',?,?,?,?,'')", rusqlite::params![id, f, fp, col]).unwrap();
        c.execute("INSERT INTO sounds_fts(rowid, filename, folder_path, collection, tags) VALUES(?,?,?,?,?)", rusqlite::params![id, f, fp, col, tag]).unwrap();
    }

    fn count(pool: &DbPool, sql: &str) -> i64 {
        pool.get().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn full_run_phases_and_counts() {
        let d = tmp("full");
        let root = d.join("lib");
        tree(&root, 30);
        std::fs::write(root.join("pack0").join("bad.wav"), b"not audio at all").unwrap();
        std::fs::write(root.join("pack0").join("notes.txt"), b"x").unwrap();
        let pool = db(&d);
        let f = vec![FolderConfig { path: root.to_string_lossy().into(), source: "sfx".into() }];
        let (res, snaps) = run(f, &pool, Arc::new(AtomicBool::new(false)), |_| false);
        let o = res.unwrap();
        assert!(!o.cancelled);
        assert_eq!(o.done, 31);
        let last = snaps.last().unwrap();
        assert_eq!(last.phase, "done");
        assert_eq!(last.status, "done");
        assert!(last.total_known && last.total == 31 && last.done == 31 && last.discovered == 31);
        assert_eq!(last.added, 31); // corrupt file is still added (without details)
        assert_eq!(last.errors, 1);
        assert_eq!(last.skipped, 0);
        assert_eq!(last.recent_errors.len(), 1);
        assert!(last.recent_errors[0].path.ends_with("bad.wav"));
        assert!(!last.can_cancel);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM sounds"), 31);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM indexed_roots"), 1);
        // phases in order; scanning snapshots have unknown total
        let order = ["scanning", "reading", "writing", "finalizing", "done"];
        let mut idx = 0;
        for s in &snaps {
            let i = order.iter().position(|p| *p == s.phase).unwrap();
            assert!(i >= idx.min(i) && (i >= idx || (i == 1 && idx == 2)), "bad order at {}", s.phase);
            idx = i;
            if s.phase == "scanning" {
                assert!(!s.total_known && s.total == 0);
            }
        }
        for p in order {
            assert!(snaps.iter().any(|s| s.phase == p), "missing phase {p}");
        }
        assert!(snaps.windows(2).all(|w| w[1].done >= w[0].done));
        // JSON shape carries legacy + new fields
        let j = serde_json::to_value(last).unwrap();
        for k in ["done", "total", "status", "phase", "total_known", "discovered", "current_root", "roots_done",
                  "roots_total", "skipped", "errors", "recent_errors", "started_at_ms", "elapsed_ms",
                  "files_per_sec", "eta_ms", "can_cancel", "bytes_done"] {
            assert!(j.get(k).is_some(), "missing {k}");
        }
    }

    #[test]
    fn cancel_midrun_keeps_library_and_user_data() {
        let d = tmp("cancel");
        let root = d.join("lib");
        tree(&root, 350);
        let pool = db(&d);
        let f = vec![FolderConfig { path: root.to_string_lossy().into(), source: "sfx".into() }];
        // first full run, then user data
        let (r, _) = run(f.clone(), &pool, Arc::new(AtomicBool::new(false)), |_| false);
        assert_eq!(r.unwrap().done, 350);
        {
            add_tag(&pool, 1, "boom");
            let c = pool.get();
            c.execute("UPDATE sounds SET favorite=1, rating=4 WHERE id=1", []).unwrap();
        }
        // an old row whose file is gone must survive a cancelled run
        pool.get().execute(
            "INSERT INTO sounds (path, filename, extension, source, indexed_at) VALUES ('/gone/old.wav','old.wav','wav','sfx','2020')", []).unwrap();
        let before = count(&pool, "SELECT COUNT(*) FROM sounds");
        let (r, snaps) = run(f.clone(), &pool, Arc::new(AtomicBool::new(false)),
            |s| s.phase == "writing" && s.done >= 100);
        let o = r.unwrap();
        assert!(o.cancelled);
        let last = snaps.last().unwrap();
        assert_eq!(last.phase, "cancelled");
        assert_eq!(last.status, "cancelled");
        assert!(last.done >= 100 && last.done < 350, "done={}", last.done);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM sounds"), before);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM sounds WHERE path='/gone/old.wav'"), 1);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM tags WHERE sound_id=1 AND tag='boom'"), 1);
        assert_eq!(count(&pool, "SELECT favorite FROM sounds WHERE id=1"), 1);
        assert_eq!(count(&pool, "SELECT rating FROM sounds WHERE id=1"), 4);
        // FTS still consistent: tag searchable
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM sounds_fts WHERE sounds_fts MATCH 'boom'"), 1);
    }

    #[test]
    fn cancel_during_scan_writes_nothing() {
        let d = tmp("cancelscan");
        let root = d.join("lib");
        tree(&root, 20);
        let pool = db(&d);
        let f = vec![FolderConfig { path: root.to_string_lossy().into(), source: "sfx".into() }];
        let (r, snaps) = run(f, &pool, Arc::new(AtomicBool::new(false)), |s| s.phase == "scanning");
        assert!(r.unwrap().cancelled);
        assert_eq!(snaps.last().unwrap().phase, "cancelled");
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM sounds"), 0);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM indexed_roots"), 0);
    }

    #[test]
    fn reindex_preserves_tags_favorites_ratings() {
        let d = tmp("reindex");
        let root = d.join("lib");
        tree(&root, 12);
        let pool = db(&d);
        let f = vec![FolderConfig { path: root.to_string_lossy().into(), source: "sfx".into() }];
        run(f.clone(), &pool, Arc::new(AtomicBool::new(false)), |_| false).0.unwrap();
        {
            add_tag(&pool, 3, "whoosh");
            let c = pool.get();
            c.execute("UPDATE sounds SET favorite=1, rating=5 WHERE id=3", []).unwrap();
        }
        let ids: i64 = count(&pool, "SELECT SUM(id) FROM sounds");
        let (r, _) = run(f, &pool, Arc::new(AtomicBool::new(false)), |_| false);
        assert_eq!(r.unwrap().done, 12);
        assert_eq!(count(&pool, "SELECT SUM(id) FROM sounds"), ids);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM tags WHERE sound_id=3 AND tag='whoosh'"), 1);
        assert_eq!(count(&pool, "SELECT favorite FROM sounds WHERE id=3"), 1);
        assert_eq!(count(&pool, "SELECT rating FROM sounds WHERE id=3"), 5);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM sounds_fts WHERE sounds_fts MATCH 'whoosh'"), 1);
    }

    #[test]
    fn unreadable_and_missing_roots_are_reported_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("errs");
        let root = d.join("lib");
        tree(&root, 5);
        let locked = root.join("pack0").join("locked.wav");
        wav(&locked, 100);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let pool = db(&d);
        let f = vec![
            FolderConfig { path: root.to_string_lossy().into(), source: "sfx".into() },
            FolderConfig { path: d.join("nope").to_string_lossy().into(), source: "sfx".into() },
        ];
        let (r, snaps) = run(f, &pool, Arc::new(AtomicBool::new(false)), |_| false);
        assert!(r.is_ok());
        let last = snaps.last().unwrap();
        assert_eq!(last.phase, "done");
        assert_eq!(last.roots_total, 2);
        assert_eq!(last.roots_done, 2);
        // missing folder error; locked file skipped unless running as root
        let is_root = std::fs::File::open(&locked).is_ok();
        assert_eq!(last.skipped, if is_root { 0 } else { 1 });
        assert!(last.recent_errors.iter().any(|e| e.reason.contains("not found")));
        assert_eq!(last.added, if is_root { 6 } else { 5 });
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn error_list_is_capped() {
        let d = tmp("cap");
        let root = d.join("lib");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..45 {
            std::fs::write(root.join(format!("bad{i}.wav")), b"junk").unwrap();
        }
        let pool = db(&d);
        let f = vec![FolderConfig { path: root.to_string_lossy().into(), source: "sfx".into() }];
        let (_, snaps) = run(f, &pool, Arc::new(AtomicBool::new(false)), |_| false);
        let last = snaps.last().unwrap();
        assert_eq!(last.errors, 45);
        assert_eq!(last.recent_errors.len(), MAX_ERRORS);
    }
}
