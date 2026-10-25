//! Library management: resolving root-relative paths safely, reveal-in-Finder, and removing
//! folders / sounds from the library (optionally moving the files to the Trash).
//!
//! The client never sends a raw filesystem path to act on. It sends a sound id, or an indexed root id plus a path
//! relative to that root; the server resolves that against `indexed_roots`, canonicalizes, and verifies the result
//! lies inside the root (no `..`, no symlink escapes).

use crate::api::AppState;
use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

type ApiErr = (StatusCode, String);

fn err(code: StatusCode, msg: impl Into<String>) -> ApiErr {
    (code, msg.into())
}

fn db_err(e: rusqlite::Error) -> ApiErr {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

// ─── Trash backend ────────────────────────────────────────────────────────────

/// Moves a file to the Trash. Never deletes permanently. Tests plug in a backend that moves into a temp dir.
pub trait TrashBackend: Send + Sync {
    fn trash(&self, path: &Path) -> Result<(), String>;
}

/// The operating system's Trash (macOS: Finder/NSFileManager via the `trash` crate).
pub struct SystemTrash;

impl TrashBackend for SystemTrash {
    fn trash(&self, path: &Path) -> Result<(), String> {
        trash::delete(path).map_err(|e| e.to_string())
    }
}

// ─── Roots and resolution ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Root {
    pub id: i64,
    /// As stored in `indexed_roots`, without a trailing slash.
    pub path: String,
    pub source: String,
}

fn trim_slash(p: &str) -> String {
    if p.len() > 1 { p.trim_end_matches('/').to_string() } else { p.to_string() }
}

pub fn load_roots(conn: &Connection) -> Result<Vec<Root>, ApiErr> {
    let mut stmt = conn.prepare("SELECT rowid, path, source FROM indexed_roots ORDER BY rowid").map_err(db_err)?;
    let rows = stmt
        .query_map([], |r| Ok(Root { id: r.get(0)?, path: trim_slash(&r.get::<_, String>(1)?), source: r.get(2)? }))
        .map_err(db_err)?
        .flatten()
        .collect();
    Ok(rows)
}

fn find_root(roots: &[Root], id: i64) -> Result<&Root, ApiErr> {
    roots.iter().find(|r| r.id == id).ok_or_else(|| err(StatusCode::NOT_FOUND, "indexed root not found"))
}

/// Validate a client-supplied root-relative path: relative, no `..`, no NUL. Returns it normalized
/// (no `.` components, no empty segments); the empty string means the root itself.
pub fn clean_rel(rel: &str) -> Result<String, ApiErr> {
    if rel.contains('\0') {
        return Err(err(StatusCode::BAD_REQUEST, "invalid path"));
    }
    let mut parts: Vec<String> = Vec::new();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return Err(err(StatusCode::BAD_REQUEST, "path must be relative to the indexed root and must not contain '..'")),
        }
    }
    Ok(parts.join("/"))
}

fn canon_root(root: &Root) -> Result<PathBuf, ApiErr> {
    std::fs::canonicalize(&root.path).map_err(|_| {
        err(StatusCode::NOT_FOUND, format!("indexed folder is not available (is the drive connected?): {}", root.path))
    })
}

/// Resolve `root` + `rel` to an absolute canonical directory that exists and lies inside the root.
/// Returns 400 for `..`/absolute input, 404 if missing, 403 if a symlink leads outside the root.
pub fn resolve_folder(root: &Root, rel: &str) -> Result<PathBuf, ApiErr> {
    let rel = clean_rel(rel)?;
    let croot = canon_root(root)?;
    let joined = if rel.is_empty() { croot.clone() } else { croot.join(&rel) };
    let canon = std::fs::canonicalize(&joined)
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("folder does not exist: {}", joined.display())))?;
    if !canon.starts_with(&croot) {
        return Err(err(StatusCode::FORBIDDEN, "path resolves outside the indexed root"));
    }
    Ok(canon)
}

/// Like `resolve_folder` but a folder that is gone from disk is not an error (library-only removal still works).
/// Returns (display path, exists).
fn resolve_folder_lenient(root: &Root, rel: &str) -> Result<(PathBuf, bool), ApiErr> {
    match resolve_folder(root, rel) {
        Ok(p) => Ok((p, true)),
        Err((StatusCode::NOT_FOUND, _)) => {
            let rel = clean_rel(rel)?;
            let p = if rel.is_empty() { PathBuf::from(&root.path) } else { Path::new(&root.path).join(rel) };
            Ok((p, false))
        }
        Err(e) => Err(e),
    }
}

/// Find the most specific root containing `path` by its stored (textual) prefix.
fn root_of<'a>(roots: &'a [Root], path: &str) -> Option<&'a Root> {
    roots
        .iter()
        .filter(|r| path.len() > r.path.len() && path.starts_with(&r.path) && (r.path == "/" || path.as_bytes()[r.path.len()] == b'/'))
        .max_by_key(|r| r.path.len())
}

/// Verify an absolute file path is a real path inside some indexed root. Returns the root and canonical path.
pub fn resolve_file(roots: &[Root], path: &str) -> Result<(Root, PathBuf), ApiErr> {
    let p = Path::new(path);
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(err(StatusCode::BAD_REQUEST, "path must be absolute and must not contain '..'"));
    }
    let canon = std::fs::canonicalize(p).map_err(|_| err(StatusCode::NOT_FOUND, format!("file does not exist: {path}")))?;
    for r in roots {
        if let Ok(cr) = std::fs::canonicalize(&r.path) {
            if canon.starts_with(&cr) && canon != cr {
                return Ok((r.clone(), canon));
            }
        }
    }
    Err(err(StatusCode::FORBIDDEN, "path is outside every indexed root"))
}

// ─── Targets ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default, Clone, Debug)]
pub struct TargetBody {
    pub kind: String,
    pub id: Option<i64>,
    pub root_id: Option<i64>,
    pub rel_path: Option<String>,
    /// Only the files directly in the root/folder, not below (the "Unsorted" node).
    pub direct_only: Option<bool>,
    /// Legacy `/api/reveal` shape: an absolute path, validated against the roots.
    pub path: Option<String>,
}

struct SoundRow {
    id: i64,
    path: String,
    size: i64,
}

struct Selection {
    /// Display path (absolute).
    path: String,
    exists: bool,
    is_root: bool,
    root: Option<Root>,
    folder_canon: Option<PathBuf>,
    sounds: Vec<SoundRow>,
}

fn select_target(conn: &Connection, roots: &[Root], t: &TargetBody) -> Result<Selection, ApiErr> {
    match t.kind.as_str() {
        "sound" => {
            let id = t.id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "id required"))?;
            let (path, size): (String, Option<i64>) = conn
                .query_row("SELECT path, size FROM sounds WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|_| err(StatusCode::NOT_FOUND, "sound not found"))?;
            let exists = Path::new(&path).exists();
            Ok(Selection {
                root: root_of(roots, &path).cloned(),
                path: path.clone(),
                exists,
                is_root: false,
                folder_canon: None,
                sounds: vec![SoundRow { id, path, size: size.unwrap_or(0) }],
            })
        }
        "folder" => {
            let root_id = t.root_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "root_id required"))?;
            let root = find_root(roots, root_id)?.clone();
            let rel = clean_rel(t.rel_path.as_deref().unwrap_or(""))?;
            let direct = t.direct_only.unwrap_or(false);
            let (abs, exists) = resolve_folder_lenient(&root, &rel)?;
            let prefix = if rel.is_empty() { format!("{}/", root.path) } else { format!("{}/{}/", root.path, rel) };
            let prefix = prefix.replacen("//", "/", 1);
            let more_specific: Vec<&Root> =
                roots.iter().filter(|r| r.id != root.id && r.path.len() > root.path.len() && r.path.starts_with(&root.path)).collect();
            let mut stmt = conn
                .prepare("SELECT id, path, COALESCE(size,0) FROM sounds WHERE substr(path, 1, ?1) = ?2")
                .map_err(db_err)?;
            let plen = prefix.chars().count() as i64;
            let sounds: Vec<SoundRow> = stmt
                .query_map(params![plen, prefix], |r| Ok(SoundRow { id: r.get(0)?, path: r.get(1)?, size: r.get(2)? }))
                .map_err(db_err)?
                .flatten()
                .filter(|s| !(direct && s.path[prefix.len()..].contains('/')))
                .filter(|s| !more_specific.iter().any(|r| s.path.starts_with(&format!("{}/", r.path))))
                .collect();
            Ok(Selection {
                path: abs.to_string_lossy().into_owned(),
                exists,
                is_root: rel.is_empty() && !direct,
                folder_canon: if exists { Some(abs) } else { None },
                root: Some(root),
                sounds,
            })
        }
        _ => Err(err(StatusCode::BAD_REQUEST, "kind must be 'folder' or 'sound'")),
    }
}

// ─── Preview / remove ─────────────────────────────────────────────────────────

#[derive(Serialize, Debug)]
pub struct Preview {
    pub path: String,
    pub sounds: usize,
    pub bytes: i64,
    pub is_root: bool,
    pub exists: bool,
}

pub fn preview(conn: &Connection, t: &TargetBody) -> Result<Preview, ApiErr> {
    let roots = load_roots(conn)?;
    let sel = select_target(conn, &roots, t)?;
    Ok(Preview {
        path: sel.path,
        sounds: sel.sounds.len(),
        bytes: sel.sounds.iter().map(|s| s.size).sum(),
        is_root: sel.is_root,
        exists: sel.exists,
    })
}

#[derive(Serialize, Debug, Clone)]
pub struct Failed {
    pub path: String,
    pub reason: String,
}

#[derive(Serialize, Debug, Default)]
pub struct RemoveOutcome {
    pub removed_sounds: usize,
    pub trashed_files: usize,
    pub failed: Vec<Failed>,
    /// Stored path of the indexed root whose `indexed_roots` row was removed, if any.
    pub removed_root: Option<String>,
}

fn is_ds_store_only(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut junk = Vec::new();
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        if e.file_name() == ".DS_Store" {
            junk.push(e.path());
        } else {
            return None;
        }
    }
    Some(junk)
}

pub fn remove(conn: &Connection, t: &TargetBody, delete_files: bool, backend: &dyn TrashBackend) -> Result<RemoveOutcome, ApiErr> {
    let roots = load_roots(conn)?;
    let sel = select_target(conn, &roots, t)?;
    let mut out = RemoveOutcome::default();
    let mut ok_ids: Vec<i64> = Vec::new();
    let mut trashed_paths: Vec<PathBuf> = Vec::new();

    if delete_files {
        let root = sel.root.clone().ok_or_else(|| err(StatusCode::FORBIDDEN, "sound is not inside an indexed root; files are left alone"))?;
        let croot = canon_root(&root)?;
        for s in &sel.sounds {
            let p = Path::new(&s.path);
            if !p.is_absolute() {
                out.failed.push(Failed { path: s.path.clone(), reason: "stored path is not absolute".into() });
                continue;
            }
            match std::fs::canonicalize(p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // already gone from disk: only the library entry remains
                    ok_ids.push(s.id);
                    continue;
                }
                Err(e) => {
                    out.failed.push(Failed { path: s.path.clone(), reason: e.to_string() });
                    continue;
                }
                Ok(c) => {
                    if !c.starts_with(&croot) || c == croot {
                        out.failed.push(Failed { path: s.path.clone(), reason: "resolves outside the indexed root".into() });
                        continue;
                    }
                    if !c.is_file() {
                        out.failed.push(Failed { path: s.path.clone(), reason: "not a regular file".into() });
                        continue;
                    }
                }
            }
            match backend.trash(p) {
                Ok(()) => {
                    ok_ids.push(s.id);
                    out.trashed_files += 1;
                    trashed_paths.push(p.to_path_buf());
                }
                Err(e) => out.failed.push(Failed { path: s.path.clone(), reason: e }),
            }
        }
    } else {
        ok_ids = sel.sounds.iter().map(|s| s.id).collect();
    }

    // One transaction for all DB removals.
    let tx = conn.unchecked_transaction().map_err(db_err)?;
    {
        // Dependents cascade (tags, suggested_tags, embeddings, radar_coords, play_history, project_sounds);
        // the sounds_ad trigger keeps the FTS index in sync.
        let mut del = tx.prepare("DELETE FROM sounds WHERE id = ?1").map_err(db_err)?;
        for id in &ok_ids {
            out.removed_sounds += del.execute([id]).map_err(db_err)?;
        }
    }
    if sel.is_root && out.failed.is_empty() {
        if let Some(r) = &sel.root {
            tx.execute("DELETE FROM indexed_roots WHERE rowid = ?1", [r.id]).map_err(db_err)?;
            out.removed_root = Some(r.path.clone());
        }
    }
    tx.commit().map_err(db_err)?;

    // Tidy now-empty directories that held trashed files (bottom-up, never the root).
    if let (Some(base), Some(root)) = (&sel.folder_canon, &sel.root) {
        if !trashed_paths.is_empty() {
            if let Ok(croot) = canon_root(root) {
                let mut dirs: Vec<PathBuf> = Vec::new();
                for f in &trashed_paths {
                    let mut d = f.parent();
                    while let Some(dir) = d {
                        let Ok(c) = std::fs::canonicalize(dir) else { break };
                        if c == croot || !c.starts_with(base) {
                            break;
                        }
                        if !dirs.contains(&dir.to_path_buf()) {
                            dirs.push(dir.to_path_buf());
                        }
                        d = dir.parent();
                    }
                }
                dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
                for d in dirs {
                    let is_real_dir = std::fs::symlink_metadata(&d).map(|m| m.is_dir()).unwrap_or(false);
                    if !is_real_dir {
                        continue;
                    }
                    if let Some(junk) = is_ds_store_only(&d) {
                        // directory is empty apart from Finder metadata: trash that too, then remove the dir
                        if junk.iter().all(|j| backend.trash(j).is_ok()) {
                            let _ = std::fs::remove_dir(&d);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

// ─── Reveal ───────────────────────────────────────────────────────────────────

/// Resolve a reveal request to an absolute, existing path inside an indexed root.
pub fn resolve_reveal(conn: &Connection, t: &TargetBody) -> Result<PathBuf, ApiErr> {
    let roots = load_roots(conn)?;
    match t.kind.as_str() {
        "folder" => {
            let root = find_root(&roots, t.root_id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "root_id required"))?)?;
            resolve_folder(root, t.rel_path.as_deref().unwrap_or(""))
        }
        "sound" => {
            let id = t.id.ok_or_else(|| err(StatusCode::BAD_REQUEST, "id required"))?;
            let path: String = conn
                .query_row("SELECT path FROM sounds WHERE id = ?1", [id], |r| r.get(0))
                .map_err(|_| err(StatusCode::NOT_FOUND, "sound not found"))?;
            resolve_file(&roots, &path).map(|(_, c)| c)
        }
        "" => match &t.path {
            // legacy shape: absolute path only, must be inside a root
            Some(p) => resolve_file(&roots, p).map(|r| r.1),
            None => Err(err(StatusCode::BAD_REQUEST, "provide kind + id / root_id + rel_path")),
        },
        _ => Err(err(StatusCode::BAD_REQUEST, "kind must be 'folder' or 'sound'")),
    }
}

async fn reveal(State(state): State<Arc<AppState>>, Json(body): Json<TargetBody>) -> Result<Json<serde_json::Value>, ApiErr> {
    let p = { resolve_reveal(&state.pool.get(), &body)? };
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg("-R").arg(&p).spawn().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open").arg(p.parent().unwrap_or(&p)).spawn().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true, "path": p.to_string_lossy() })))
}


// ─── Rename / move ────────────────────────────────────────────────────────────

/// Rename or move a sound within its own indexed root. The destination is canonicalized (via its parent), must stay
/// inside the same root, must not exist, and a new name may not contain separators or `..`.
pub fn rename(conn: &Connection, id: i64, new_name: Option<&str>, new_path: Option<&str>) -> Result<serde_json::Value, ApiErr> {
    let roots = load_roots(conn)?;
    let old: String = conn
        .query_row("SELECT path FROM sounds WHERE id = ?1", [id], |r| r.get(0))
        .map_err(|_| err(StatusCode::NOT_FOUND, "sound not found"))?;
    let (root, old_canon) = resolve_file(&roots, &old)?;
    let croot = canon_root(&root)?;

    let (dest_parent, file_name): (PathBuf, String) = match (new_name, new_path) {
        (_, Some(np)) => {
            let p = Path::new(np);
            if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
                return Err(err(StatusCode::BAD_REQUEST, "new_path must be absolute and must not contain '..'"));
            }
            let name = p.file_name().ok_or_else(|| err(StatusCode::BAD_REQUEST, "new_path has no file name"))?.to_string_lossy().into_owned();
            let parent = p.parent().ok_or_else(|| err(StatusCode::BAD_REQUEST, "new_path has no parent"))?;
            (parent.to_path_buf(), name)
        }
        (Some(n), None) => (old_canon.parent().unwrap_or(&croot).to_path_buf(), n.to_string()),
        _ => return Err(err(StatusCode::BAD_REQUEST, "provide new_name or new_path")),
    };
    if file_name.is_empty() || file_name == "." || file_name == ".." || file_name.contains(['/', '\\', '\0']) || file_name.len() > 255 {
        return Err(err(StatusCode::BAD_REQUEST, "invalid file name"));
    }
    let canon_parent = std::fs::canonicalize(&dest_parent).map_err(|_| err(StatusCode::NOT_FOUND, "destination folder does not exist"))?;
    if !canon_parent.starts_with(&croot) {
        return Err(err(StatusCode::FORBIDDEN, "destination outside the sound's indexed root"));
    }
    let dest = canon_parent.join(&file_name);
    if std::fs::symlink_metadata(&dest).is_ok() {
        return Err(err(StatusCode::CONFLICT, "destination already exists"));
    }
    std::fs::rename(&old_canon, &dest).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Same spelling as the indexer: absolute path under the stored root, folder_path/collection relative to it.
    let rel_parent = canon_parent.strip_prefix(&croot).unwrap_or(Path::new("")).to_string_lossy().into_owned();
    let new_abs = if rel_parent.is_empty() { format!("{}/{}", root.path, file_name) } else { format!("{}/{}/{}", root.path, rel_parent, file_name) };
    let folder: Option<String> = if rel_parent.is_empty() { None } else { Some(rel_parent.clone()) };
    let collection: Option<String> = folder.as_ref().map(|f| f.split('/').next().unwrap_or("").to_string());
    let ext = Path::new(&file_name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if let Err(e) = conn.execute(
        "UPDATE sounds SET path = ?1, filename = ?2, extension = ?3, folder_path = ?4, collection = ?5 WHERE id = ?6",
        params![new_abs, file_name, ext, folder, collection, id],
    ) {
        let _ = std::fs::rename(&dest, &old_canon); // keep disk and DB consistent
        return Err(db_err(e));
    }
    Ok(serde_json::json!({
        "ok": true, "id": id, "path": new_abs, "filename": file_name,
        "folder_path": folder.unwrap_or_default(), "collection": collection,
    }))
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn preview_handler(State(state): State<Arc<AppState>>, Json(body): Json<TargetBody>) -> Result<Json<Preview>, ApiErr> {
    let conn = state.pool.get();
    preview(&conn, &body).map(Json)
}

#[derive(Deserialize)]
struct RemoveBody {
    #[serde(flatten)]
    target: TargetBody,
    #[serde(default)]
    delete_files: bool,
}

pub fn forget_root(state: &AppState, root_path: &str) {
    state.indexed_roots.lock().unwrap().retain(|r| trim_slash(r) != root_path);
}

async fn remove_handler(State(state): State<Arc<AppState>>, Json(body): Json<RemoveBody>) -> Result<Json<RemoveOutcome>, ApiErr> {
    let out = {
        let conn = state.pool.get();
        remove(&conn, &body.target, body.delete_files, &SystemTrash)?
    };
    if let Some(r) = &out.removed_root {
        forget_root(&state, r);
    }
    Ok(Json(out))
}

/// Used by the single-sound delete endpoint: always the Trash, never a permanent delete.
pub fn delete_sound(state: &AppState, id: i64) -> Result<RemoveOutcome, ApiErr> {
    let conn = state.pool.get();
    let t = TargetBody { kind: "sound".into(), id: Some(id), ..Default::default() };
    remove(&conn, &t, true, &SystemTrash)
}

#[derive(Serialize)]
struct RootInfo {
    id: i64,
    path: String,
    source: String,
    sounds: usize,
    exists: bool,
}

async fn list_roots(State(state): State<Arc<AppState>>) -> Result<Json<serde_json::Value>, ApiErr> {
    let conn = state.pool.get();
    let roots = load_roots(&conn)?;
    let mut counts: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut stmt = conn.prepare("SELECT path FROM sounds").map_err(db_err)?;
    for p in stmt.query_map([], |r| r.get::<_, String>(0)).map_err(db_err)?.flatten() {
        if let Some(r) = root_of(&roots, &p) {
            *counts.entry(r.id).or_default() += 1;
        }
    }
    let infos: Vec<RootInfo> = roots
        .iter()
        .map(|r| RootInfo { id: r.id, path: r.path.clone(), source: r.source.clone(), sounds: counts.get(&r.id).copied().unwrap_or(0), exists: Path::new(&r.path).is_dir() })
        .collect();
    Ok(Json(serde_json::json!({ "roots": infos })))
}

/// Make a user-typed root absolute and canonical for indexing. A path already registered keeps its stored spelling
/// so existing rows are not duplicated.
pub fn normalize_root_input(conn: &Connection, input: &str) -> Result<String, String> {
    let t = trim_slash(input.trim());
    if !Path::new(&t).is_absolute() {
        return Err(format!("folder path must be absolute: {input}"));
    }
    if let Ok(roots) = load_roots(conn) {
        if roots.iter().any(|r| r.path == t) {
            return Ok(t);
        }
    }
    Ok(std::fs::canonicalize(&t).map(|p| p.to_string_lossy().into_owned()).unwrap_or(t))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/reveal", post(reveal))
        .route("/api/library/roots", get(list_roots))
        .route("/api/library/remove_preview", post(preview_handler))
        .route("/api/library/remove", post(remove_handler))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    struct FakeTrash {
        dir: PathBuf,
        fail_names: Vec<String>,
    }
    impl TrashBackend for FakeTrash {
        fn trash(&self, p: &Path) -> Result<(), String> {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if self.fail_names.contains(&name) {
                return Err("simulated failure".into());
            }
            let n = N.fetch_add(1, Ordering::SeqCst);
            std::fs::rename(p, self.dir.join(format!("{n}-{name}"))).map_err(|e| e.to_string())
        }
    }

    struct Env {
        base: PathBuf,
        root: PathBuf,
        trash: PathBuf,
        conn: Connection,
    }

    fn env(name: &str) -> Env {
        let base = PathBuf::from(format!("/tmp/daouchi-work/library/{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = std::fs::canonicalize({
            std::fs::create_dir_all(base.join("lib")).unwrap();
            base.join("lib")
        })
        .unwrap();
        let trash = base.join("trash");
        std::fs::create_dir_all(&trash).unwrap();
        let conn = Connection::open(base.join("t.db")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(include_str!("../../schema.sql")).unwrap();
        conn.execute("INSERT INTO indexed_roots(path, source) VALUES (?1, 'sfx')", [root.to_string_lossy()]).unwrap();
        Env { base, root, trash, conn }
    }

    impl Env {
        fn add(&self, rel: &str, size: usize) -> i64 {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![0u8; size]).unwrap();
            let comps: Vec<&str> = rel.split('/').collect();
            let coll = if comps.len() > 1 { Some(comps[0].to_string()) } else { None };
            let folder = if comps.len() > 1 { Some(comps[..comps.len() - 1].join("/")) } else { None };
            self.conn
                .execute(
                    "INSERT INTO sounds(path, filename, extension, size, collection, folder_path, source, indexed_at) VALUES (?1,?2,'wav',?3,?4,?5,'sfx','now')",
                    params![p.to_string_lossy(), comps.last().unwrap(), size as i64, coll, folder],
                )
                .unwrap();
            let id = self.conn.last_insert_rowid();
            self.conn.execute("INSERT INTO tags(sound_id, tag) VALUES (?1,'boom')", [id]).unwrap();
            // keep the FTS row in step with the tag, as the tag endpoints do
            self.conn.execute("INSERT INTO sounds_fts(sounds_fts, rowid, filename, folder_path, collection, tags) SELECT 'delete', id, filename, folder_path, collection, '' FROM sounds WHERE id=?1", [id]).unwrap();
            self.conn.execute("INSERT INTO sounds_fts(rowid, filename, folder_path, collection, tags) SELECT id, filename, folder_path, collection, 'boom' FROM sounds WHERE id=?1", [id]).unwrap();
            self.conn.execute("INSERT INTO suggested_tags(sound_id, tag, score) VALUES (?1,'x',0.5)", [id]).unwrap();
            self.conn.execute("INSERT INTO embeddings(sound_id, vector) VALUES (?1, x'00')", [id]).unwrap();
            self.conn.execute("INSERT INTO radar_coords(sound_id,x,y) VALUES (?1,0,0)", [id]).unwrap();
            self.conn.execute("INSERT OR IGNORE INTO projects(id,name,created_at) VALUES (1,'p','now')", []).unwrap();
            self.conn.execute("INSERT INTO project_sounds(project_id,sound_id,added_at) VALUES (1,?1,'now')", [id]).unwrap();
            id
        }
        fn count(&self, table: &str) -> i64 {
            self.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        }
        fn rid(&self) -> i64 {
            load_roots(&self.conn).unwrap()[0].id
        }
        fn folder(&self, rel: &str) -> TargetBody {
            TargetBody { kind: "folder".into(), root_id: Some(self.rid()), rel_path: Some(rel.into()), ..Default::default() }
        }
        fn ft(&self) -> FakeTrash {
            FakeTrash { dir: self.trash.clone(), fail_names: vec![] }
        }
        fn fts_ok(&self) {
            self.conn.execute("INSERT INTO sounds_fts(sounds_fts) VALUES('integrity-check')", []).unwrap();
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn resolves_relative_to_root_not_cwd() {
        let e = env("resolve");
        e.add("Coll/a/x.wav", 10);
        let root = &load_roots(&e.conn).unwrap()[0];
        assert_eq!(resolve_folder(root, "Coll").unwrap(), e.root.join("Coll"));
        assert_eq!(resolve_folder(root, "Coll/a").unwrap(), e.root.join("Coll/a"));
        assert_eq!(resolve_folder(root, "").unwrap(), e.root);
        assert_eq!(resolve_reveal(&e.conn, &e.folder("Coll")).unwrap(), e.root.join("Coll"));
        let missing = resolve_folder(root, "Nope").unwrap_err();
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn rejects_dotdot_absolute_and_symlink_escape() {
        let e = env("escape");
        e.add("Coll/x.wav", 1);
        let outside = e.base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("o.wav"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, e.root.join("Coll/link")).unwrap();
        let root = &load_roots(&e.conn).unwrap()[0];
        assert_eq!(resolve_folder(root, "../outside").unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(resolve_folder(root, "Coll/../../outside").unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(resolve_folder(root, "/etc").unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(resolve_folder(root, "Coll/link").unwrap_err().0, StatusCode::FORBIDDEN);
        // legacy absolute path outside roots / symlink file escape
        let roots = load_roots(&e.conn).unwrap();
        assert!(resolve_file(&roots, outside.join("o.wav").to_str().unwrap()).is_err());
        assert!(resolve_file(&roots, e.root.join("Coll/link/o.wav").to_str().unwrap()).is_err());
        assert!(resolve_file(&roots, e.root.join("Coll/x.wav").to_str().unwrap()).is_ok());
        assert!(resolve_file(&roots, "Coll/x.wav").is_err());
        let t = TargetBody { kind: "folder".into(), root_id: Some(999), rel_path: Some("".into()), ..Default::default() };
        assert_eq!(preview(&e.conn, &t).unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn preview_counts_and_bytes() {
        let e = env("preview");
        e.add("A/x.wav", 100);
        e.add("A/sub/y.wav", 200);
        e.add("B/z.wav", 400);
        e.add("top.wav", 800);
        let p = preview(&e.conn, &e.folder("A")).unwrap();
        assert_eq!((p.sounds, p.bytes, p.is_root), (2, 300, false));
        assert_eq!(p.path, e.root.join("A").to_string_lossy());
        let p = preview(&e.conn, &e.folder("")).unwrap();
        assert_eq!((p.sounds, p.bytes, p.is_root), (4, 1500, true));
        let mut t = e.folder("");
        t.direct_only = Some(true);
        let p = preview(&e.conn, &t).unwrap();
        assert_eq!((p.sounds, p.bytes, p.is_root), (1, 800, false));
        let id = e.add("B/q.wav", 5);
        let p = preview(&e.conn, &TargetBody { kind: "sound".into(), id: Some(id), ..Default::default() }).unwrap();
        assert_eq!((p.sounds, p.bytes), (1, 5));
        // prefix must not match sibling "A2"
        e.add("A2/w.wav", 7);
        assert_eq!(preview(&e.conn, &e.folder("A")).unwrap().sounds, 2);
    }

    #[test]
    fn remove_from_library_only_keeps_files() {
        let e = env("libonly");
        e.add("A/x.wav", 1);
        e.add("A/sub/y.wav", 1);
        e.add("B/z.wav", 1);
        let out = remove(&e.conn, &e.folder("A"), false, &e.ft()).unwrap();
        assert_eq!((out.removed_sounds, out.trashed_files), (2, 0));
        assert!(e.root.join("A/x.wav").exists() && e.root.join("A/sub/y.wav").exists());
        assert_eq!(e.count("sounds"), 1);
        for t in ["tags", "suggested_tags", "embeddings", "radar_coords", "project_sounds"] {
            assert_eq!(e.count(t), 1, "{t}");
        }
        assert_eq!(e.count("indexed_roots"), 1);
        e.fts_ok();
    }

    #[test]
    fn remove_with_trash_removes_dirs_and_root_row() {
        let e = env("trash");
        e.add("A/x.wav", 1);
        e.add("A/sub/y.wav", 1);
        e.add("B/z.wav", 1);
        std::fs::write(e.root.join("A/sub/notaudio.txt"), b"keep").unwrap();
        let out = remove(&e.conn, &e.folder("A"), true, &e.ft()).unwrap();
        assert_eq!((out.removed_sounds, out.trashed_files), (2, 2));
        assert!(!e.root.join("A/x.wav").exists());
        assert!(e.root.join("A/sub/notaudio.txt").exists(), "unknown files untouched");
        assert!(e.root.join("A/sub").exists());
        assert!(e.root.join("B/z.wav").exists());
        assert_eq!(std::fs::read_dir(&e.trash).unwrap().count(), 2);
        // second folder: dirs become empty and are removed, root kept
        let out = remove(&e.conn, &e.folder("B"), true, &e.ft()).unwrap();
        assert_eq!(out.trashed_files, 1);
        assert!(!e.root.join("B").exists());
        assert!(e.root.exists());
        // whole root
        e.add("C/c.wav", 1);
        let out = remove(&e.conn, &e.folder(""), true, &e.ft()).unwrap();
        assert_eq!(out.removed_sounds, 1);
        assert_eq!(out.removed_root.as_deref(), Some(e.root.to_str().unwrap()));
        assert!(e.root.exists(), "root dir is never removed");
        assert_eq!(e.count("indexed_roots"), 0);
        assert_eq!(e.count("sounds"), 0);
        e.fts_ok();
    }

    #[test]
    fn partial_failure_keeps_rows() {
        let e = env("partial");
        let ok = e.add("A/ok.wav", 1);
        let bad = e.add("A/bad.wav", 1);
        let t = FakeTrash { dir: e.trash.clone(), fail_names: vec!["bad.wav".into()] };
        let out = remove(&e.conn, &e.folder(""), true, &t).unwrap();
        assert_eq!((out.removed_sounds, out.trashed_files), (1, 1));
        assert_eq!(out.failed.len(), 1);
        assert!(out.failed[0].path.ends_with("bad.wav"));
        assert!(out.removed_root.is_none(), "root row stays while files remain");
        assert_eq!(e.count("indexed_roots"), 1);
        let left: Vec<i64> = e.conn.prepare("SELECT id FROM sounds").unwrap().query_map([], |r| r.get(0)).unwrap().flatten().collect();
        assert_eq!(left, vec![bad]);
        assert_ne!(ok, bad);
        assert!(e.root.join("A/bad.wav").exists());
        assert!(e.root.join("A").exists());
    }

    #[test]
    fn symlink_escape_is_not_trashed() {
        let e = env("symtrash");
        let outside = e.base.join("outside.wav");
        std::fs::write(&outside, b"x").unwrap();
        e.add("A/x.wav", 1);
        std::os::unix::fs::symlink(&outside, e.root.join("A/link.wav")).unwrap();
        e.conn
            .execute(
                "INSERT INTO sounds(path, filename, extension, size, collection, folder_path, source, indexed_at) VALUES (?1,'link.wav','wav',1,'A','A','sfx','now')",
                [e.root.join("A/link.wav").to_string_lossy()],
            )
            .unwrap();
        let out = remove(&e.conn, &e.folder("A"), true, &e.ft()).unwrap();
        assert_eq!(out.failed.len(), 1);
        assert!(outside.exists());
        assert_eq!(e.count("sounds"), 1);
    }

    #[test]
    fn single_sound_and_missing_file() {
        let e = env("single");
        let id = e.add("A/x.wav", 1);
        std::fs::remove_file(e.root.join("A/x.wav")).unwrap();
        let t = TargetBody { kind: "sound".into(), id: Some(id), ..Default::default() };
        let out = remove(&e.conn, &t, true, &e.ft()).unwrap();
        assert_eq!((out.removed_sounds, out.trashed_files, out.failed.len()), (1, 0, 0));
        assert_eq!(remove(&e.conn, &t, true, &e.ft()).unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn normalize_root_input_rules() {
        let e = env("norm");
        assert!(normalize_root_input(&e.conn, "relative/dir").is_err());
        let with_dots = format!("{}/../lib/", e.root.display());
        assert_eq!(normalize_root_input(&e.conn, &with_dots).unwrap(), e.root.to_string_lossy());
    }

    #[test]
    fn rename_rules() {
        let e = env("rename");
        let id = e.add("A/x.wav", 1);
        e.add("A/taken.wav", 1);
        e.add("B/y.wav", 1);
        for bad in ["../evil.wav", "a/b.wav", "..", "", "a\\b.wav"] {
            assert_eq!(rename(&e.conn, id, Some(bad), None).unwrap_err().0, StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(rename(&e.conn, id, Some("taken.wav"), None).unwrap_err().0, StatusCode::CONFLICT);
        let outside = e.base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        assert_eq!(rename(&e.conn, id, None, Some(outside.join("z.wav").to_str().unwrap())).unwrap_err().0, StatusCode::FORBIDDEN);
        std::os::unix::fs::symlink(&outside, e.root.join("A/esc")).unwrap();
        assert_eq!(rename(&e.conn, id, None, Some(e.root.join("A/esc/z.wav").to_str().unwrap())).unwrap_err().0, StatusCode::FORBIDDEN);
        let dotdot = format!("{}/A/../../outside/z.wav", e.root.display());
        assert_eq!(rename(&e.conn, id, None, Some(&dotdot)).unwrap_err().0, StatusCode::BAD_REQUEST);
        assert!(e.root.join("A/x.wav").exists() && !outside.join("z.wav").exists());
    }

    #[test]
    fn rename_and_move_update_db_and_fts() {
        let e = env("rename-ok");
        let id = e.add("A/x.wav", 1);
        let r = rename(&e.conn, id, Some("renamed.wav"), None).unwrap();
        assert_eq!(r["path"], e.root.join("A/renamed.wav").to_string_lossy().as_ref());
        assert!(e.root.join("A/renamed.wav").exists() && !e.root.join("A/x.wav").exists());
        std::fs::create_dir_all(e.root.join("B")).unwrap();
        let r = rename(&e.conn, id, None, Some(e.root.join("B/moved.wav").to_str().unwrap())).unwrap();
        assert_eq!((r["folder_path"].as_str(), r["collection"].as_str()), (Some("B"), Some("B")));
        let (p, f, c): (String, String, String) =
            e.conn.query_row("SELECT path, filename, collection FROM sounds WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((p.as_str(), f.as_str(), c.as_str()), (e.root.join("B/moved.wav").to_str().unwrap(), "moved.wav", "B"));
        let hits: i64 = e.conn.query_row("SELECT COUNT(*) FROM sounds_fts WHERE sounds_fts MATCH 'moved'", [], |r| r.get(0)).unwrap();
        assert_eq!(hits, 1);
        e.fts_ok();
    }
}
