use crate::api::AppState;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use rusqlite::types::ToSql;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Full sound record returned by the API.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Sound {
    pub id: i64,
    pub path: String,
    pub filename: String,
    pub extension: String,
    pub size: Option<i64>,
    pub duration: Option<f64>,
    pub bitrate: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub collection: Option<String>,
    pub folder_path: Option<String>,
    pub source: Option<String>,
    pub favorite: bool,
    pub tags: Vec<String>,
    pub has_embedding: bool,
    pub has_stems: bool,
    pub indexed_at: String,
    pub file_modified_at: Option<String>,
    pub rating: i64,
    pub label: i64,
}

#[derive(Deserialize, Default)]
pub struct SearchParams {
    pub q: Option<String>,
    pub source: Option<String>,
    pub ext: Option<String>,
    pub dur_min: Option<f64>,
    pub dur_max: Option<f64>,
    pub favorite: Option<bool>,
    #[serde(default)]
    pub tag: Vec<String>,
    pub bundle_id: Option<i64>,
    /// Folder filter: matches collection name OR folder_path (including sub-paths).
    pub folder: Option<String>,
    pub label: Option<i64>,
    pub min_rating: Option<i64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub sort: Option<String>,
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub results: Vec<Sound>,
    pub total: i64,
}

/// Sanitize a free-text query: keep alphanumeric, spaces, hyphens.
fn sanitize_query(q: &str) -> String {
    q.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Build an FTS5-safe match string from a sanitized query.
/// Appends `*` to the last token so the search is prefix-match on the tail.
fn build_fts_query(q: &str) -> String {
    let tokens: Vec<&str> = q.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = tokens[..tokens.len() - 1]
        .iter()
        .map(|t| t.to_string())
        .collect();
    parts.push(format!("{}*", tokens[tokens.len() - 1]));
    parts.join(" ")
}

/// Fetch tags for a list of sound ids in a single query.
pub fn fetch_tags_for_ids(
    conn: &rusqlite::Connection,
    ids: &[i64],
) -> std::collections::HashMap<i64, Vec<String>> {
    let mut map: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
    if ids.is_empty() {
        return map;
    }

    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT sound_id, tag FROM tags WHERE sound_id IN ({placeholders}) ORDER BY tag"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return map,
    };

    if let Ok(iter) = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    }) {
        for item in iter.flatten() {
            map.entry(item.0).or_default().push(item.1);
        }
    }
    map
}

/// Check which sound ids have embeddings.
pub fn fetch_embedding_flags(
    conn: &rusqlite::Connection,
    ids: &[i64],
) -> std::collections::HashSet<i64> {
    let mut set = std::collections::HashSet::new();
    if ids.is_empty() {
        return set;
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT sound_id FROM embeddings WHERE model = '{}' AND sound_id IN ({placeholders})",
        crate::engines::CURRENT_MODEL
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return set,
    };
    if let Ok(iter) =
        stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| r.get::<_, i64>(0))
    {
        for id in iter.flatten() {
            set.insert(id);
        }
    }
    set
}

/// Check whether a sound has all four stem files on disk (not just the directory).
pub fn has_stems_on_disk(path: &str) -> bool {
    let hash = blake3::hash(path.as_bytes()).to_hex();
    let dir = crate::lifecycle::stems_cache_dir().join(hash.as_str());
    ["drums.wav", "bass.wav", "vocals.wav", "other.wav"]
        .iter()
        .all(|f| dir.join(f).exists())
}

pub fn row_to_sound(row: &rusqlite::Row) -> rusqlite::Result<Sound> {
    let favorite_int: i64 = row.get(13)?;
    Ok(Sound {
        id: row.get(0)?,
        path: row.get(1)?,
        filename: row.get(2)?,
        extension: row.get(3)?,
        size: row.get(4)?,
        duration: row.get(5)?,
        bitrate: row.get(6)?,
        sample_rate: row.get(7)?,
        bit_depth: row.get(8)?,
        collection: row.get(9)?,
        folder_path: row.get(10)?,
        source: row.get(11)?,
        indexed_at: row.get(12)?,
        file_modified_at: row.get(14).ok(),
        rating: row.get(15).unwrap_or(0),
        label: row.get(16).unwrap_or(0),
        favorite: favorite_int != 0,
        tags: vec![],
        has_embedding: false,
        has_stems: false,
    })
}

pub const SOUND_COLS: &str =
    "s.id, s.path, s.filename, s.extension, s.size, s.duration, s.bitrate, \
     s.sample_rate, s.bit_depth, s.collection, s.folder_path, s.source, s.indexed_at, s.favorite, \
     s.file_modified_at, s.rating, s.label";

/// Executes a query with a dynamic list of params stored as `Vec<Box<dyn ToSql>>`.
/// We encode each value variant explicitly to avoid trait-object ToSql issues.
#[derive(Clone)]
enum SqlVal {
    Text(String),
    Real(f64),
    Int(i64),
}

impl ToSql for SqlVal {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            SqlVal::Text(s) => s.to_sql(),
            SqlVal::Real(f) => f.to_sql(),
            SqlVal::Int(i) => i.to_sql(),
        }
    }
}

async fn search_sounds(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Json<SearchResponse> {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let offset = params.offset.unwrap_or(0).max(0);

    let mut where_clauses: Vec<String> = Vec::new();
    let mut bind_vals: Vec<SqlVal> = Vec::new();

    let use_fts = params.q.as_deref().map(|q| !q.trim().is_empty()).unwrap_or(false);

    if use_fts {
        let raw = sanitize_query(params.q.as_deref().unwrap_or(""));
        let fts_q = build_fts_query(&raw);
        where_clauses
            .push("s.id IN (SELECT rowid FROM sounds_fts WHERE sounds_fts MATCH ?)".to_string());
        bind_vals.push(SqlVal::Text(fts_q));
    }

    if let Some(src) = &params.source {
        where_clauses.push("s.source = ?".to_string());
        bind_vals.push(SqlVal::Text(src.clone()));
    }
    if let Some(ext) = &params.ext {
        where_clauses.push("s.extension = ?".to_string());
        bind_vals.push(SqlVal::Text(ext.clone()));
    }
    if let Some(dmin) = params.dur_min {
        where_clauses.push("s.duration >= ?".to_string());
        bind_vals.push(SqlVal::Real(dmin));
    }
    if let Some(dmax) = params.dur_max {
        where_clauses.push("s.duration <= ?".to_string());
        bind_vals.push(SqlVal::Real(dmax));
    }
    if let Some(fav) = params.favorite {
        where_clauses.push("s.favorite = ?".to_string());
        bind_vals.push(SqlVal::Int(if fav { 1 } else { 0 }));
    }
    if !params.tag.is_empty() {
        // AND semantics: each tag gets its own subquery clause joined by AND,
        // so only sounds that have ALL selected tags are returned.
        for t in &params.tag {
            where_clauses.push(
                "s.id IN (SELECT sound_id FROM tags WHERE tag = ?)".to_string(),
            );
            bind_vals.push(SqlVal::Text(t.clone()));
        }
    }
    if let Some(bundle_id) = params.bundle_id {
        where_clauses.push(
            "s.id IN (SELECT sound_id FROM tags WHERE tag IN \
             (SELECT tag FROM bundle_tags WHERE bundle_id = ?))"
                .to_string(),
        );
        bind_vals.push(SqlVal::Int(bundle_id));
    }
    if let Some(folder) = &params.folder {
        // Match collection name (top-level click) OR exact folder_path OR any sub-path.
        // Escape LIKE special characters in folder to prevent unintended wildcard matches.
        let escaped_folder = folder.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        where_clauses.push(
            "(s.collection = ? OR s.folder_path = ? OR s.folder_path LIKE ? ESCAPE '\\')".to_string(),
        );
        bind_vals.push(SqlVal::Text(folder.clone()));
        bind_vals.push(SqlVal::Text(folder.clone()));
        bind_vals.push(SqlVal::Text(format!("{escaped_folder}/%")));
    }
    if let Some(label) = params.label {
        if label > 0 {
            where_clauses.push("s.label = ?".to_string());
            bind_vals.push(SqlVal::Int(label));
        }
    }
    if let Some(min_rating) = params.min_rating {
        if min_rating > 0 {
            where_clauses.push("s.rating >= ?".to_string());
            bind_vals.push(SqlVal::Int(min_rating));
        }
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };

    let order_sql = match params.sort.as_deref() {
        Some("date_asc")      => "s.indexed_at ASC",
        Some("name_asc")      => "s.filename ASC",
        Some("name_desc")     => "s.filename DESC",
        Some("duration_asc")  => "s.duration ASC",
        Some("duration_desc") => "s.duration DESC",
        Some("size_desc")     => "s.size DESC",
        Some("modified_desc") => "COALESCE(s.file_modified_at, s.indexed_at) DESC",
        Some("modified_asc")  => "COALESCE(s.file_modified_at, s.indexed_at) ASC",
        _                     => "s.indexed_at DESC",
    };

    let count_sql = format!("SELECT COUNT(*) FROM sounds s {where_sql}");
    let data_sql = format!(
        "SELECT {SOUND_COLS} FROM sounds s {where_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?"
    );

    // Perform all DB work in a scoped block so that the MutexGuard (`conn`) is
    // dropped before the first .await below. MutexGuard is !Send and holding it
    // across an await point would make the future !Send.
    let (total, mut sounds) = {
        let conn = state.pool.get();

        // Count total
        let total: i64 = {
            let mut stmt = match conn.prepare(&count_sql) {
                Ok(s) => s,
                Err(_) => return Json(SearchResponse { results: vec![], total: 0 }),
            };
            stmt.query_row(rusqlite::params_from_iter(bind_vals.iter()), |r| r.get(0))
                .unwrap_or(0)
        };

        // Fetch rows
        let mut sounds: Vec<Sound> = {
            let mut stmt = match conn.prepare(&data_sql) {
                Ok(s) => s,
                Err(_) => return Json(SearchResponse { results: vec![], total }),
            };

            let mut all_params = bind_vals.clone();
            all_params.push(SqlVal::Int(limit));
            all_params.push(SqlVal::Int(offset));

            stmt.query_map(rusqlite::params_from_iter(all_params.iter()), row_to_sound)
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };

        let ids: Vec<i64> = sounds.iter().map(|s| s.id).collect();
        let tags_map = fetch_tags_for_ids(&conn, &ids);
        let embed_set = fetch_embedding_flags(&conn, &ids);

        for sound in &mut sounds {
            sound.tags = tags_map.get(&sound.id).cloned().unwrap_or_default();
            sound.has_embedding = embed_set.contains(&sound.id);
        }
        // conn (MutexGuard) drops here at end of block
        (total, sounds)
    };

    // has_stems_on_disk performs 4 stat(2) syscalls per sound. Run them on the
    // blocking thread pool so they do not stall the async executor — especially
    // important for network-mounted audio libraries.
    let sounds = tokio::task::spawn_blocking(move || {
        for sound in &mut sounds {
            sound.has_stems = has_stems_on_disk(&sound.path);
        }
        sounds
    })
    .await
    .unwrap_or_default();

    Json(SearchResponse {
        results: sounds,
        total,
    })
}

async fn get_sound(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Sound>, (axum::http::StatusCode, String)> {
    // Perform all DB work in a scoped block so the MutexGuard is dropped before
    // the spawn_blocking().await below (MutexGuard is !Send).
    let mut s = {
        let conn = state.pool.get();

        let sound = {
            let mut stmt = conn
                .prepare(&format!("SELECT {SOUND_COLS} FROM sounds s WHERE s.id = ?"))
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            stmt.query_row([id], row_to_sound)
                .map_err(|_| (axum::http::StatusCode::NOT_FOUND, "not found".to_string()))?
        };

        let ids = [sound.id];
        let tags_map = fetch_tags_for_ids(&conn, &ids);
        let embed_set = fetch_embedding_flags(&conn, &ids);

        let mut s = sound;
        s.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
        s.has_embedding = embed_set.contains(&s.id);
        // conn (MutexGuard) drops here at end of block
        s
    };

    // Run the blocking stat(2) calls off the async executor.
    let s = tokio::task::spawn_blocking(move || {
        s.has_stems = has_stems_on_disk(&s.path);
        s
    })
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(s))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sounds/stats", get(stats))
        .route("/api/sounds/recent", get(recent_sounds))
        .route("/api/sounds/search", get(search_sounds))
        .route("/api/sounds/semantic_search", axum::routing::post(semantic_search))
        .route("/api/sounds/similar/:id", get(similar_sounds))
        .route("/api/sounds/:id", get(get_sound))
}

async fn stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let conn = state.pool.get();

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM sounds", [], |r| r.get(0))
        .unwrap_or(0);
    let total_duration: f64 = conn
        .query_row("SELECT COALESCE(SUM(duration),0) FROM sounds", [], |r| r.get(0))
        .unwrap_or(0.0);
    let sfx_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sounds WHERE source='sfx'", [], |r| r.get(0))
        .unwrap_or(0);
    let music_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sounds WHERE source='music'", [], |r| r.get(0))
        .unwrap_or(0);
    let with_embeddings: i64 = conn
        .query_row("SELECT COUNT(*) FROM embeddings WHERE model = ?1", [crate::engines::CURRENT_MODEL], |r| r.get(0))
        .unwrap_or(0);
    // Stem files live on disk (demucs cache), not in the DB.
    // Cannot count them accurately via SQL; rely on per-sound has_stems flag in search results.
    let with_stems: i64 = 0;
    let tag_count: i64 = conn
        .query_row("SELECT COUNT(DISTINCT tag) FROM tags", [], |r| r.get(0))
        .unwrap_or(0);

    // Collection breakdown
    let collections: Vec<serde_json::Value> = {
        let mut stmt = conn
            .prepare(
                "SELECT collection, COUNT(*), COALESCE(AVG(duration),0), source \
                 FROM sounds GROUP BY collection ORDER BY COUNT(*) DESC LIMIT 30",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "name": r.get::<_, Option<String>>(0)?.unwrap_or_else(|| "Unknown".into()),
                "count": r.get::<_, i64>(1)?,
                "avg_duration": r.get::<_, f64>(2)?,
                "source": r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            }))
        })
        .unwrap()
        .flatten()
        .collect()
    };

    // Tag frequency
    let top_tags: Vec<serde_json::Value> = {
        let mut stmt = conn
            .prepare(
                "SELECT tag, COUNT(*) as freq FROM tags GROUP BY tag ORDER BY freq DESC LIMIT 50",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "tag": r.get::<_, String>(0)?,
                "count": r.get::<_, i64>(1)?,
            }))
        })
        .unwrap()
        .flatten()
        .collect()
    };

    Json(serde_json::json!({
        "total": total,
        "total_duration": total_duration,
        "sfx_count": sfx_count,
        "music_count": music_count,
        "with_embeddings": with_embeddings,
        "with_stems": with_stems,
        "tag_count": tag_count,
        "collections": collections,
        "top_tags": top_tags,
    }))
}

#[derive(Deserialize, Default)]
struct RecentParams {
    limit: Option<i64>,
}

async fn recent_sounds(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RecentParams>,
) -> Json<SearchResponse> {
    let limit = params.limit.unwrap_or(10).clamp(1, 500);
    let sql = format!(
        "SELECT {SOUND_COLS} FROM sounds s ORDER BY s.indexed_at DESC LIMIT ?",
    );

    let (mut sounds, tags_map, embed_set) = {
        let conn = state.pool.get();
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Json(SearchResponse { results: vec![], total: 0 }),
        };
        let sounds: Vec<Sound> = stmt
            .query_map([limit], row_to_sound)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default();
        let ids: Vec<i64> = sounds.iter().map(|s| s.id).collect();
        let tags_map = fetch_tags_for_ids(&conn, &ids);
        let embed_set = fetch_embedding_flags(&conn, &ids);
        (sounds, tags_map, embed_set)
    };

    for sound in &mut sounds {
        sound.tags = tags_map.get(&sound.id).cloned().unwrap_or_default();
        sound.has_embedding = embed_set.contains(&sound.id);
    }

    let sounds = tokio::task::spawn_blocking(move || {
        for sound in &mut sounds {
            sound.has_stems = has_stems_on_disk(&sound.path);
        }
        sounds
    })
    .await
    .unwrap_or_default();

    let total = sounds.len() as i64;
    Json(SearchResponse { results: sounds, total })
}

#[derive(serde::Deserialize)]
struct SemanticSearchBody {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize { 50 }

async fn semantic_search(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SemanticSearchBody>,
) -> Result<Json<SearchResponse>, crate::api::ApiError> {
    let results = state
        .ai
        .semantic_search(&body.query, body.limit)
        .await?;

    if results.is_empty() {
        return Ok(Json(SearchResponse { results: vec![], total: 0 }));
    }

    let ids: Vec<i64> = results.iter().map(|r| r.sound_id).collect();
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT {SOUND_COLS} FROM sounds s WHERE s.id IN ({placeholders})"
    );

    // Collect all DB work in a single lock acquisition so no MutexGuard is held
    // across an .await point (MutexGuard is !Send and would make the future !Send).
    let (mut sounds, tags_map, embed_set) = {
        let conn = state.pool.get();
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let sounds: Vec<Sound> = stmt.query_map(rusqlite::params_from_iter(ids.iter()), row_to_sound)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default();
        let sound_ids: Vec<i64> = sounds.iter().map(|s| s.id).collect();
        let tags_map = fetch_tags_for_ids(&conn, &sound_ids);
        let embed_set = fetch_embedding_flags(&conn, &sound_ids);
        (sounds, tags_map, embed_set)
    };

    for sound in &mut sounds {
        sound.tags = tags_map.get(&sound.id).cloned().unwrap_or_default();
        sound.has_embedding = embed_set.contains(&sound.id);
    }

    // Run the blocking stat(2) calls off the async executor.
    let mut sounds = tokio::task::spawn_blocking(move || {
        for sound in &mut sounds {
            sound.has_stems = has_stems_on_disk(&sound.path);
        }
        sounds
    })
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Re-order by semantic score
    let score_map: std::collections::HashMap<i64, f64> =
        results.iter().map(|r| (r.sound_id, r.score)).collect();
    sounds.sort_by(|a, b| {
        score_map.get(&b.id).unwrap_or(&0.0)
            .partial_cmp(score_map.get(&a.id).unwrap_or(&0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total = sounds.len() as i64;
    Ok(Json(SearchResponse { results: sounds, total }))
}

#[derive(Deserialize, Default)]
struct SimilarParams {
    limit: Option<usize>,
}

async fn similar_sounds(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(params): Query<SimilarParams>,
) -> Result<Json<SearchResponse>, (axum::http::StatusCode, String)> {
    let limit = params.limit.unwrap_or(30).min(200);

    // All DB work inside a single block so conn (MutexGuard) drops before .await
    let (scored, ids, score_map, sounds_raw, tags_map, embed_set) = {
        let conn = state.pool.get();

        let query_blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT vector FROM embeddings WHERE sound_id = ?1 AND model = ?2",
                rusqlite::params![id, crate::engines::CURRENT_MODEL],
                |r| r.get(0),
            )
            .ok();

        let query_blob = match query_blob {
            Some(b) => b,
            None => {
                return Err((
                    axum::http::StatusCode::NOT_FOUND,
                    "No embedding for this sound".into(),
                ))
            }
        };

        let query_vec: Vec<f32> = query_blob
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();

        let q_norm: f32 = query_vec.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
        let query_unit: Vec<f32> = query_vec.iter().map(|x| x / q_norm).collect();

        let all_rows: Vec<(i64, Vec<u8>)> = {
            let mut stmt = conn
                .prepare("SELECT sound_id, vector FROM embeddings WHERE sound_id != ?1 AND model = ?2")
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            stmt.query_map(rusqlite::params![id, crate::engines::CURRENT_MODEL], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };

        let mut scored: Vec<(i64, f32)> = all_rows
            .into_iter()
            .filter_map(|(sid, blob)| {
                if blob.len() != query_unit.len() * 4 {
                    return None;
                }
                let vec: Vec<f32> = blob
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
                let dot: f32 = vec.iter().zip(&query_unit).map(|(a, b)| a / norm * b).sum();
                Some((sid, dot))
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        if scored.is_empty() {
            return Ok(Json(SearchResponse { results: vec![], total: 0 }));
        }

        let ids: Vec<i64> = scored.iter().map(|(sid, _)| *sid).collect();
        let score_map: std::collections::HashMap<i64, f64> =
            scored.iter().map(|(sid, s)| (*sid, *s as f64)).collect();

        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!("SELECT {SOUND_COLS} FROM sounds s WHERE s.id IN ({placeholders})");

        let sounds_raw: Vec<Sound> = {
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            stmt.query_map(rusqlite::params_from_iter(ids.iter()), row_to_sound)
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };

        let sound_ids: Vec<i64> = sounds_raw.iter().map(|s| s.id).collect();
        let tags_map = fetch_tags_for_ids(&conn, &sound_ids);
        let embed_set = fetch_embedding_flags(&conn, &sound_ids);

        (scored, ids, score_map, sounds_raw, tags_map, embed_set)
    }; // conn dropped here

    let _ = (scored, ids); // silence unused warnings

    let mut sounds = sounds_raw;
    for sound in &mut sounds {
        sound.tags = tags_map.get(&sound.id).cloned().unwrap_or_default();
        sound.has_embedding = embed_set.contains(&sound.id);
    }

    let mut sounds = tokio::task::spawn_blocking(move || {
        for sound in &mut sounds {
            sound.has_stems = has_stems_on_disk(&sound.path);
        }
        sounds
    })
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    sounds.sort_by(|a, b| {
        score_map.get(&b.id).unwrap_or(&0.0)
            .partial_cmp(score_map.get(&a.id).unwrap_or(&0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total = sounds.len() as i64;
    Ok(Json(SearchResponse { results: sounds, total }))
}
