use crate::api::AppState;
use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ─── Favorites ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct FavoriteResponse {
    favorite: bool,
}

async fn toggle_favorite(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<FavoriteResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();

    // Single atomic UPDATE…RETURNING avoids a read-modify-write race.
    let new_val: i64 = conn
        .query_row(
            "UPDATE sounds SET favorite = CASE WHEN favorite = 1 THEN 0 ELSE 1 END \
             WHERE id = ? RETURNING favorite",
            [id],
            |r| r.get(0),
        )
        .map_err(|_| (axum::http::StatusCode::NOT_FOUND, "not found".to_string()))?;

    Ok(Json(FavoriteResponse {
        favorite: new_val != 0,
    }))
}

// ─── Tags ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TagsBody {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

#[derive(Serialize)]
struct TagsResponse {
    tags: Vec<String>,
}

async fn update_tags(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<TagsBody>,
) -> Result<Json<TagsResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();

    // Snapshot tags BEFORE mutation — the FTS 'delete' command must receive the
    // exact values that were previously indexed so it can decrement the right
    // posting lists.  Passing tags='' causes ghost term entries to accumulate.
    let old_tags_joined: String = {
        let mut stmt = conn
            .prepare("SELECT tag FROM tags WHERE sound_id = ? ORDER BY tag")
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        stmt.query_map([id], |r| r.get::<_, String>(0))
            .map(|rows| rows.flatten().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };

    conn.execute_batch("BEGIN")
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let result: rusqlite::Result<Vec<String>> = (|| {
        for tag in &body.add {
            conn.execute(
                "INSERT OR IGNORE INTO tags (sound_id, tag) VALUES (?1, ?2)",
                rusqlite::params![id, tag],
            )?;
        }

        for tag in &body.remove {
            conn.execute(
                "DELETE FROM tags WHERE sound_id = ?1 AND tag = ?2",
                rusqlite::params![id, tag],
            )?;
        }

        let tags_str: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT tag FROM tags WHERE sound_id = ? ORDER BY tag")?;
            stmt.query_map([id], |r| r.get(0))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };
        let tags_joined = tags_str.join(" ");

        let (filename, folder_path, collection): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT filename, folder_path, collection FROM sounds WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;

        conn.execute(
            "INSERT INTO sounds_fts(sounds_fts, rowid, filename, folder_path, collection, tags) \
             VALUES('delete', ?, ?, ?, ?, ?)",
            rusqlite::params![id, filename, folder_path, collection, old_tags_joined],
        )
        .map_err(|e| {
            tracing::warn!("FTS delete failed for sound_id={id}: {e}");
            e
        })?;
        conn.execute(
            "INSERT INTO sounds_fts(rowid, filename, folder_path, collection, tags) \
             VALUES(?, ?, ?, ?, ?)",
            rusqlite::params![id, filename, folder_path, collection, tags_joined],
        )
        .map_err(|e| {
            tracing::warn!("FTS insert failed for sound_id={id}: {e}");
            e
        })?;

        Ok(tags_str)
    })();

    let tags_str = match result {
        Ok(tags) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            tags
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };

    Ok(Json(TagsResponse { tags: tags_str }))
}

async fn get_all_tags(State(state): State<Arc<AppState>>) -> Result<Json<TagsResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    let mut stmt = conn
        .prepare("SELECT DISTINCT tag FROM tags ORDER BY tag")
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let tags = stmt
        .query_map([], |r| r.get(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    Ok(Json(TagsResponse { tags }))
}

#[derive(Serialize)]
struct ConfirmTagResponse {
    tag: String,
    confirmed: bool,
}

async fn confirm_suggested_tag(
    State(state): State<Arc<AppState>>,
    Path((id, tag)): Path<(i64, String)>,
) -> Result<Json<ConfirmTagResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();

    // Snapshot current tags before mutation so FTS 'delete' has accurate prior values.
    let old_tags_joined: String = {
        let mut stmt = conn
            .prepare("SELECT tag FROM tags WHERE sound_id = ? ORDER BY tag")
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        stmt.query_map([id], |r| r.get::<_, String>(0))
            .map(|rows| rows.flatten().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };

    conn.execute_batch("BEGIN")
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let result: rusqlite::Result<()> = (|| {
        conn.execute(
            "INSERT OR IGNORE INTO tags (sound_id, tag) VALUES (?1, ?2)",
            rusqlite::params![id, tag],
        )?;

        conn.execute(
            "DELETE FROM suggested_tags WHERE sound_id = ?1 AND tag = ?2",
            rusqlite::params![id, tag],
        )?;

        let tags_str: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT tag FROM tags WHERE sound_id = ? ORDER BY tag")?;
            stmt.query_map([id], |r| r.get(0))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };
        let tags_joined = tags_str.join(" ");

        let (filename, folder_path, collection): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT filename, folder_path, collection FROM sounds WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;

        conn.execute(
            "INSERT INTO sounds_fts(sounds_fts, rowid, filename, folder_path, collection, tags) \
             VALUES('delete', ?, ?, ?, ?, ?)",
            rusqlite::params![id, filename, folder_path, collection, old_tags_joined],
        )
        .map_err(|e| {
            tracing::warn!("FTS delete failed for sound_id={id}: {e}");
            e
        })?;
        conn.execute(
            "INSERT INTO sounds_fts(rowid, filename, folder_path, collection, tags) \
             VALUES(?, ?, ?, ?, ?)",
            rusqlite::params![id, filename, folder_path, collection, tags_joined],
        )
        .map_err(|e| {
            tracing::warn!("FTS insert failed for sound_id={id}: {e}");
            e
        })?;

        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    }

    Ok(Json(ConfirmTagResponse {
        tag,
        confirmed: true,
    }))
}

#[derive(Serialize)]
struct DismissResponse {
    dismissed: bool,
}

async fn dismiss_suggested_tag(
    State(state): State<Arc<AppState>>,
    Path((id, tag)): Path<(i64, String)>,
) -> Result<Json<DismissResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();

    conn.execute(
        "DELETE FROM suggested_tags WHERE sound_id = ?1 AND tag = ?2",
        rusqlite::params![id, tag],
    )
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(DismissResponse { dismissed: true }))
}

// ─── Suggested tags GET ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct SuggestedTagItem {
    tag: String,
    score: f64,
}

#[derive(Serialize)]
struct SuggestedTagsResponse {
    suggestions: Vec<SuggestedTagItem>,
}

async fn get_suggested_tags(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<SuggestedTagsResponse>, (axum::http::StatusCode, String)> {
    // Fetch existing suggestions from DB
    let suggestions = {
        let conn = state.pool.get();
        let mut stmt = conn
            .prepare(
                "SELECT tag, score FROM suggested_tags \
                 WHERE sound_id = ? ORDER BY score DESC",
            )
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        stmt.query_map([id], |r| {
            Ok(SuggestedTagItem {
                tag:   r.get(0)?,
                score: r.get(1)?,
            })
        })
        .map(|rows| rows.flatten().collect::<Vec<_>>())
        .unwrap_or_default()
    };

    // If none exist yet, kick off AST tagging in the background
    if suggestions.is_empty() {
        let path: Option<String> = {
            let conn = state.pool.get();
            conn.query_row("SELECT path FROM sounds WHERE id = ?", [id], |r| r.get(0))
                .ok()
        };
        if let Some(p) = path {
            let ai = state.ai.clone();
            tokio::spawn(async move {
                if let Err(e) = ai.autotag(id, &p).await {
                    eprintln!("[autotag] sound_id={id}: {e}");
                }
            });
        }
    }

    Ok(Json(SuggestedTagsResponse { suggestions }))
}

async fn clear_suggested_tags(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    conn.execute(
        "DELETE FROM suggested_tags WHERE sound_id = ?1",
        rusqlite::params![id],
    )
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "cleared": true })))
}

// ─── Folders ──────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct FolderNode {
    /// Unique node key (root id + filter path); use this for list keys.
    key: String,
    name: String,
    /// Filter key for `GET /api/sounds/search?folder=` (collection name or root-relative folder path).
    path: String,
    source: Option<String>,
    count: i64,
    /// Indexed root this node belongs to (null for legacy rows not under any registered root).
    root_id: Option<i64>,
    root_path: Option<String>,
    /// Path relative to the root; "" with `direct_only` = files sitting directly in the root.
    rel_path: String,
    direct_only: bool,
    /// Absolute path for display only. Actions must send `root_id` + `rel_path`.
    abs_path: Option<String>,
    children: Vec<FolderNode>,
}

#[derive(Serialize)]
struct FoldersResponse {
    tree: Vec<FolderNode>,
}

async fn get_folders(State(state): State<Arc<AppState>>) -> Result<Json<FoldersResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    let roots = crate::api::library::load_roots(&conn)?;
    let root_path = |id: Option<i64>| id.and_then(|i| roots.iter().find(|r| r.id == i)).map(|r| r.path.clone());

    // (root id, folder_path, collection, source, count). The root is the most specific indexed root containing the file.
    let mut stmt = conn
        .prepare(
            "SELECT (SELECT r.rowid FROM indexed_roots r \
                      WHERE substr(s.path, 1, length(rtrim(r.path,'/')) + 1) = rtrim(r.path,'/') || '/' \
                      ORDER BY length(r.path) DESC LIMIT 1) AS rid, \
                    COALESCE(s.folder_path,''), COALESCE(s.collection,''), s.source, COUNT(*) \
             FROM sounds s GROUP BY rid, s.folder_path, s.collection, s.source ORDER BY s.folder_path",
        )
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    struct Row {
        rid: Option<i64>,
        folder_path: String,
        collection: String,
        source: Option<String>,
        count: i64,
    }

    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok(Row { rid: r.get(0)?, folder_path: r.get(1)?, collection: r.get(2)?, source: r.get(3)?, count: r.get(4)? })
        })
        .map(|iter| iter.flatten().collect())
        .unwrap_or_default();

    let mk = |rid: Option<i64>, name: String, path: String, rel: String, direct: bool, source: Option<String>, count: i64| {
        let rp = root_path(rid);
        let abs = rp.as_ref().map(|r| if rel.is_empty() { r.clone() } else { format!("{r}/{rel}") });
        FolderNode {
            key: format!("{}|{}", rid.unwrap_or(0), path),
            name, path, source, count, root_id: rid, root_path: rp, rel_path: rel, direct_only: direct, abs_path: abs,
            children: vec![],
        }
    };

    // Two-level tree per (root, collection): collection -> folder_path.
    let mut tree: std::collections::BTreeMap<(String, Option<i64>), FolderNode> = std::collections::BTreeMap::new();
    for row in &rows {
        let unsorted = row.collection.is_empty();
        let coll = if unsorted { "Unsorted".to_string() } else { row.collection.clone() };
        let node = tree.entry((coll.clone(), row.rid)).or_insert_with(|| {
            let rel = if unsorted { String::new() } else { coll.clone() };
            mk(row.rid, coll.clone(), coll.clone(), rel, unsorted, row.source.clone(), 0)
        });
        node.count += row.count;
        if !row.folder_path.is_empty() && row.folder_path != coll {
            let leaf = row.folder_path.rsplit('/').next().unwrap_or(&row.folder_path).to_string();
            node.children.push(mk(row.rid, leaf, row.folder_path.clone(), row.folder_path.clone(), false, row.source.clone(), row.count));
        }
    }

    Ok(Json(FoldersResponse { tree: tree.into_values().collect() }))
}

// ─── Tag Bundles ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Bundle {
    id: i64,
    name: String,
    tags: Vec<String>,
    created_at: String,
}

#[derive(Deserialize)]
struct CreateBundleBody {
    name: String,
}

#[derive(Serialize)]
struct BundlesResponse {
    bundles: Vec<Bundle>,
}

#[derive(Serialize)]
struct DeletedResponse {
    deleted: bool,
}

async fn create_bundle(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateBundleBody>,
) -> Result<Json<Bundle>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO tag_bundles (name, created_at) VALUES (?1, ?2)",
        rusqlite::params![body.name, now],
    )
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let id = conn.last_insert_rowid();

    Ok(Json(Bundle {
        id,
        name: body.name,
        tags: vec![],
        created_at: now,
    }))
}

async fn list_bundles(State(state): State<Arc<AppState>>) -> Result<Json<BundlesResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();

    // Step 1: fetch all bundle rows first (so stmt is dropped before next prepare)
    let bundle_rows: Vec<(i64, String, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, name, created_at FROM tag_bundles ORDER BY created_at DESC")
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    };

    // Step 2: for each bundle fetch tags in a separate query
    let mut bundles: Vec<Bundle> = Vec::with_capacity(bundle_rows.len());
    for (id, name, created_at) in bundle_rows {
        let tags = {
            let mut s = conn
                .prepare("SELECT tag FROM bundle_tags WHERE bundle_id = ? ORDER BY tag")
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            s.query_map([id], |r| r.get(0))
                .map(|i| i.flatten().collect())
                .unwrap_or_default()
        };
        bundles.push(Bundle {
            id,
            name,
            tags,
            created_at,
        });
    }

    Ok(Json(BundlesResponse { bundles }))
}

async fn update_bundle_tags(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<TagsBody>,
) -> Result<Json<TagsResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();

    // Verify the bundle exists before attempting add/remove so that FK
    // violations (which produce 500) are replaced with a correct 404.
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM tag_bundles WHERE id = ?",
            [id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !exists {
        return Err((axum::http::StatusCode::NOT_FOUND, "bundle not found".to_string()));
    }

    for tag in &body.add {
        conn.execute(
            "INSERT OR IGNORE INTO bundle_tags (bundle_id, tag) VALUES (?1, ?2)",
            rusqlite::params![id, tag],
        )
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    for tag in &body.remove {
        conn.execute(
            "DELETE FROM bundle_tags WHERE bundle_id = ?1 AND tag = ?2",
            rusqlite::params![id, tag],
        )
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let mut stmt = conn
        .prepare("SELECT tag FROM bundle_tags WHERE bundle_id = ? ORDER BY tag")
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let tags: Vec<String> = stmt
        .query_map([id], |r| r.get(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();

    Ok(Json(TagsResponse { tags }))
}

async fn delete_bundle(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<DeletedResponse>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();

    let n = conn
        .execute("DELETE FROM tag_bundles WHERE id = ?", [id])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if n == 0 {
        return Err((axum::http::StatusCode::NOT_FOUND, "bundle not found".to_string()));
    }

    Ok(Json(DeletedResponse { deleted: true }))
}

// ─── Rating ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RatingBody {
    rating: i64,   // 0 = unrated, 1–5 = stars
}

#[derive(Serialize)]
struct RatingResponse {
    rating: i64,
}

async fn set_rating(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<RatingBody>,
) -> Result<Json<RatingResponse>, (axum::http::StatusCode, String)> {
    let r = body.rating.clamp(0, 5);
    let conn = state.pool.get();
    let rows = conn
        .execute("UPDATE sounds SET rating = ?1 WHERE id = ?2", rusqlite::params![r, id])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if rows == 0 {
        return Err((axum::http::StatusCode::NOT_FOUND, "not found".to_string()));
    }
    Ok(Json(RatingResponse { rating: r }))
}

// ─── Label ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LabelBody {
    label: i64,   // 0 = no label, 1–5 = color index
}

#[derive(Serialize)]
struct LabelResponse {
    label: i64,
}

async fn set_label(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<LabelBody>,
) -> Result<Json<LabelResponse>, (axum::http::StatusCode, String)> {
    let l = body.label.clamp(0, 5);
    let conn = state.pool.get();
    let rows = conn
        .execute("UPDATE sounds SET label = ?1 WHERE id = ?2", rusqlite::params![l, id])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if rows == 0 {
        return Err((axum::http::StatusCode::NOT_FOUND, "not found".to_string()));
    }
    Ok(Json(LabelResponse { label: l }))
}



#[derive(Deserialize)]
struct BulkTagsBody {
    sound_ids: Vec<i64>,
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

#[derive(Serialize)]
struct BulkTagsResponse {
    updated: usize,
}

async fn bulk_update_tags(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BulkTagsBody>,
) -> Result<Json<BulkTagsResponse>, (axum::http::StatusCode, String)> {
    if body.sound_ids.is_empty() || (body.add.is_empty() && body.remove.is_empty()) {
        return Ok(Json(BulkTagsResponse { updated: 0 }));
    }

    let conn = state.pool.get();
    let mut updated = 0usize;

    for id in &body.sound_ids {
        let old_tags_joined: String = {
            let mut stmt = conn
                .prepare("SELECT tag FROM tags WHERE sound_id = ? ORDER BY tag")
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            stmt.query_map([id], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        };

        conn.execute_batch("BEGIN")
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let result: rusqlite::Result<Vec<String>> = (|| {
            for tag in &body.add {
                conn.execute(
                    "INSERT OR IGNORE INTO tags (sound_id, tag) VALUES (?1, ?2)",
                    rusqlite::params![id, tag],
                )?;
            }
            for tag in &body.remove {
                conn.execute(
                    "DELETE FROM tags WHERE sound_id = ?1 AND tag = ?2",
                    rusqlite::params![id, tag],
                )?;
            }

            let tags_str: Vec<String> = {
                let mut stmt = conn
                    .prepare("SELECT tag FROM tags WHERE sound_id = ? ORDER BY tag")?;
                stmt.query_map([id], |r| r.get(0))
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default()
            };
            let tags_joined = tags_str.join(" ");

            let (filename, folder_path, collection): (String, Option<String>, Option<String>) =
                conn.query_row(
                    "SELECT filename, folder_path, collection FROM sounds WHERE id = ?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;

            conn.execute(
                "INSERT INTO sounds_fts(sounds_fts, rowid, filename, folder_path, collection, tags) \
                 VALUES('delete', ?, ?, ?, ?, ?)",
                rusqlite::params![id, filename, folder_path, collection, old_tags_joined],
            )?;
            conn.execute(
                "INSERT INTO sounds_fts(rowid, filename, folder_path, collection, tags) \
                 VALUES(?, ?, ?, ?, ?)",
                rusqlite::params![id, filename, folder_path, collection, tags_joined],
            )?;
            Ok(tags_str)
        })();

        match result {
            Ok(_) => {
                conn.execute_batch("COMMIT")
                    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                updated += 1;
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
        }
    }

    Ok(Json(BulkTagsResponse { updated }))
}



// ─── Delete sound from disk ────────────────────────────────────────────────────

/// Body is accepted for backwards compatibility; the file always goes to the Trash (never a permanent delete).
#[derive(serde::Deserialize, Default)]
#[allow(dead_code)]
struct DeleteSoundBody {
    trash: Option<bool>,
}

async fn delete_sound_from_disk(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    body: Option<Json<DeleteSoundBody>>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _ = body;
    let out = crate::api::library::delete_sound(&state, id)?;
    if let Some(f) = out.failed.first() {
        return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("could not move to Trash: {} ({})", f.path, f.reason)));
    }
    Ok(Json(serde_json::json!({"deleted": true, "id": id, "trashed": out.trashed_files > 0})))
}

// ─── Rename / Move sound on disk ──────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RenameSoundBody {
    new_name: Option<String>,       // rename: just change filename, same dir
    new_path: Option<String>,       // move: full absolute destination path (same indexed root)
}

async fn rename_sound(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<RenameSoundBody>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    crate::api::library::rename(&conn, id, body.new_name.as_deref(), body.new_path.as_deref()).map(Json)
}

// ─── Transients ───────────────────────────────────────────────────────────────

async fn get_transients_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let path: String = {
        let conn = state.pool.get();
        conn.query_row("SELECT path FROM sounds WHERE id = ?", [id], |r| r.get(0))
            .map_err(|_| (axum::http::StatusCode::NOT_FOUND, "sound not found".to_string()))?
    };
    let fracs: Vec<f32> = tokio::task::spawn_blocking(move || {
        daouchi_ai::transients::detect(std::path::Path::new(&path)).unwrap_or_else(|e| {
            eprintln!("[transients] sound_id={id}: {e:#}");
            Vec::new()
        })
    })
    .await
    .unwrap_or_default();
    Ok(Json(serde_json::json!({ "sound_id": id, "fractions": fracs })))
}

// ─── Artwork ───────────────────────────────────────────────────────────────────

/// Extract the first embedded picture (APIC / PICTURE block / iTunes cover art)
/// from an audio file using symphonia. Returns (bytes, mime_type) or None.
fn extract_embedded_art(path: &str) -> Option<(Vec<u8>, String)> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let src = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());
    let meta_opts = MetadataOptions {
        limit_metadata_bytes: symphonia::core::meta::Limit::Maximum(10_000_000),
        ..Default::default()
    };

    let mut probed = symphonia::default::get_probe()
        .format(&Hint::new(), mss, &FormatOptions::default(), &meta_opts)
        .ok()?;

    // Metadata may sit in the probe result or in the format reader; check both.
    let visuals_from_probe: Vec<(Vec<u8>, String)> = probed
        .metadata
        .get()
        .and_then(|log| log.current().map(|rev| {
            rev.visuals()
                .iter()
                .filter(|v| !v.data.is_empty())
                .map(|v| (v.data.to_vec(), v.media_type.clone()))
                .collect::<Vec<_>>()
        }))
        .unwrap_or_default();

    if let Some((data, mime)) = visuals_from_probe.into_iter().next() {
        return Some((data, mime));
    }

    // Also check the format reader's metadata log
    if let Some(rev) = probed.format.metadata().current() {
        if let Some(v) = rev.visuals().iter().find(|v| !v.data.is_empty()) {
            return Some((v.data.to_vec(), v.media_type.clone()));
        }
    }

    None
}

/// Generate a procedural SVG artwork based on filename + collection.
/// The color is derived by hashing the collection/filename so each sound
/// gets a unique but stable color identity.
fn generate_artwork_svg(filename: &str, extension: &str, collection: &str) -> String {
    // Deterministic hue from filename hash
    let hash: u32 = filename
        .bytes()
        .chain(collection.bytes())
        .fold(2166136261u32, |acc, b| acc.wrapping_mul(16777619).wrapping_add(b as u32));

    let hue         = hash % 360;
    let hue2        = (hue + 50) % 360;
    let hue3        = (hue + 180) % 360;

    // Derive wave amplitude variation seed
    let seed = (hash >> 8) & 0xff;
    let amp1 = 18 + (seed & 0x0f) as i32;       // 18–33
    let amp2 = 10 + ((seed >> 4) & 0x0f) as i32; // 10–25

    // Clamp filename for display
    let stem = filename.trim_end_matches(&format!(".{}", extension));
    let label: String = if stem.chars().count() > 22 {
        format!("{}…", stem.chars().take(22).collect::<String>())
    } else {
        stem.to_string()
    };
    let ext_upper = extension.to_uppercase();

    // Build abstract wave path — multiple sine-like bezier curves
    let w = 400i32;
    let cy = 200i32;
    let steps = 8;
    let step_w = w / steps;
    let mut path1 = format!("M 0 {cy}");
    let mut path2 = format!("M 0 {cy}");
    for i in 0..steps {
        let x0 = i * step_w;
        let x1 = x0 + step_w / 2;
        let x2 = x0 + step_w;
        let dir: i32 = if i % 2 == 0 { 1 } else { -1 };
        path1.push_str(&format!(" Q {x1} {} {x2} {cy}", cy + dir * amp1));
        path2.push_str(&format!(" Q {x1} {} {x2} {cy}", cy - dir * amp2));
    }

    format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 400 400" width="400" height="400">
  <defs>
    <radialGradient id="bg" cx="50%" cy="50%" r="70%">
      <stop offset="0%"   stop-color="hsl({hue2},55%,16%)"/>
      <stop offset="100%" stop-color="hsl({hue3},40%,8%)"/>
    </radialGradient>
    <linearGradient id="wave1" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0%"   stop-color="hsl({hue},70%,60%)" stop-opacity="0"/>
      <stop offset="40%"  stop-color="hsl({hue},70%,60%)" stop-opacity="0.9"/>
      <stop offset="100%" stop-color="hsl({hue},70%,60%)" stop-opacity="0.2"/>
    </linearGradient>
    <linearGradient id="wave2" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0%"   stop-color="hsl({hue2},65%,55%)" stop-opacity="0.1"/>
      <stop offset="60%"  stop-color="hsl({hue2},65%,55%)" stop-opacity="0.7"/>
      <stop offset="100%" stop-color="hsl({hue2},65%,55%)" stop-opacity="0"/>
    </linearGradient>
    <filter id="blur"><feGaussianBlur stdDeviation="3"/></filter>
  </defs>
  <rect width="400" height="400" fill="url(#bg)"/>
  <!-- glow blob -->
  <ellipse cx="200" cy="200" rx="130" ry="90"
           fill="hsl({hue},60%,30%)" opacity="0.18" filter="url(#blur)"/>
  <!-- wave lines -->
  <path d="{path1}" stroke="url(#wave1)" stroke-width="2.5" fill="none" opacity="0.85"/>
  <path d="{path2}" stroke="url(#wave2)" stroke-width="1.5" fill="none" opacity="0.7"/>
  <!-- bottom bar -->
  <rect x="0" y="350" width="400" height="50"
        fill="hsl({hue3},35%,6%)" opacity="0.85"/>
  <!-- filename -->
  <text x="16" y="378" font-family="system-ui,-apple-system,sans-serif"
        font-size="13" font-weight="600" fill="rgba(255,255,255,0.9)"
        dominant-baseline="middle">{label}</text>
  <!-- extension badge -->
  <rect x="{badge_x}" y="363" width="{badge_w}" height="18" rx="4"
        fill="hsl({hue},55%,40%)" opacity="0.9"/>
  <text x="{badge_tx}" y="372" font-family="system-ui,-apple-system,sans-serif"
        font-size="9" font-weight="700" fill="white" dominant-baseline="middle"
        text-anchor="middle">{ext_upper}</text>
</svg>"#,
        hue = hue, hue2 = hue2, hue3 = hue3,
        path1 = path1, path2 = path2,
        label = label,
        ext_upper = ext_upper,
        badge_w  = ext_upper.len() as i32 * 7 + 10,
        badge_x  = 400 - ext_upper.len() as i32 * 7 - 26,
        badge_tx = 400 - ext_upper.len() as i32 * 7 / 2 - 13,
    )
}

async fn get_artwork(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<axum::response::Response<axum::body::Body>, (axum::http::StatusCode, String)> {
    use axum::{body::Body, http::header, response::Response};

    // Resolve sound path + metadata from DB
    let (path, filename, extension, collection): (String, String, String, String) = {
        let conn = state.pool.get();
        conn.query_row(
            "SELECT path, filename, extension, COALESCE(collection,'') FROM sounds WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(|_| (axum::http::StatusCode::NOT_FOUND, "sound not found".to_string()))?
    };

    let cache_dir = crate::lifecycle::cache_dir().join("artwork");
    let _ = std::fs::create_dir_all(&cache_dir);

    // Check disk cache (prefer raster over svg)
    for (ext, mime) in &[("jpg", "image/jpeg"), ("png", "image/png"), ("svg", "image/svg+xml")] {
        let p = cache_dir.join(format!("{id}.{ext}"));
        if p.exists() {
            let bytes = tokio::fs::read(&p).await
                .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            return Ok(Response::builder()
                .header(header::CONTENT_TYPE, *mime)
                .header(header::CACHE_CONTROL, "max-age=31536000, immutable")
                .body(Body::from(bytes))
                .unwrap());
        }
    }

    // Try to extract embedded artwork (blocking I/O)
    let path_clone = path.clone();
    let embedded = tokio::task::spawn_blocking(move || extract_embedded_art(&path_clone))
        .await
        .unwrap_or(None);

    if let Some((bytes, media_type)) = embedded {
        let ext = if media_type.contains("png") { "png" } else { "jpg" };
        let cache_path = cache_dir.join(format!("{id}.{ext}"));
        let _ = tokio::fs::write(&cache_path, &bytes).await;
        return Ok(Response::builder()
            .header(header::CONTENT_TYPE, media_type)
            .header(header::CACHE_CONTROL, "max-age=31536000, immutable")
            .body(Body::from(bytes))
            .unwrap());
    }

    // Generate procedural SVG
    let svg = generate_artwork_svg(&filename, &extension, &collection);
    let cache_path = cache_dir.join(format!("{id}.svg"));
    let _ = tokio::fs::write(&cache_path, &svg).await;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "image/svg+xml")
        .header(header::CACHE_CONTROL, "max-age=31536000, immutable")
        .body(Body::from(svg))
        .unwrap())
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sounds/:id/favorite", post(toggle_favorite))
        .route("/api/sounds/:id/rating", post(set_rating))
        .route("/api/sounds/:id/label", post(set_label))
        .route("/api/sounds/:id/tags", post(update_tags))
        .route("/api/sounds/bulk_tags", post(bulk_update_tags))
        .route("/api/tags", get(get_all_tags))
        .route("/api/sounds/:id/suggested_tags", get(get_suggested_tags).delete(clear_suggested_tags))
        .route(
            "/api/sounds/:id/suggested_tags/:tag/confirm",
            post(confirm_suggested_tag),
        )
        .route(
            "/api/sounds/:id/suggested_tags/:tag",
            delete(dismiss_suggested_tag),
        )
        .route("/api/folders", get(get_folders))
        .route("/api/bundles", post(create_bundle).get(list_bundles))
        .route("/api/bundles/:id/tags", post(update_bundle_tags))
        .route("/api/bundles/:id", delete(delete_bundle))
        .route("/api/sounds/:id/delete", post(delete_sound_from_disk))
        .route("/api/sounds/:id/rename", post(rename_sound))
        .route("/api/sounds/:id/transients", get(get_transients_handler))
        .route("/api/sounds/:id/artwork",    get(get_artwork))
}
