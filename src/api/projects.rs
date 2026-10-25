use crate::api::AppState;
use crate::api::search::{
    fetch_embedding_flags, fetch_tags_for_ids, has_stems_on_disk, row_to_sound, Sound, SOUND_COLS,
};
use axum::{
    extract::{Path, State},
    routing::{delete, get},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize)]
struct Project {
    id: i64,
    name: String,
    color: String,
    sound_count: i64,
    created_at: String,
}

#[derive(Deserialize)]
struct CreateProjectBody {
    name: String,
    #[serde(default = "default_color")]
    color: String,
}

fn default_color() -> String {
    "#e47a2a".to_string()
}

#[derive(Deserialize)]
struct AddSoundsBody {
    sound_ids: Vec<i64>,
}

async fn list_projects(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<Project>> {
    let conn = state.pool.get();
    let mut stmt = match conn.prepare(
        "SELECT p.id, p.name, p.color, p.created_at, COUNT(ps.sound_id) \
         FROM projects p \
         LEFT JOIN project_sounds ps ON ps.project_id = p.id \
         GROUP BY p.id ORDER BY p.created_at DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Json(vec![]),
    };
    let projects = stmt
        .query_map([], |r| {
            Ok(Project {
                id:          r.get(0)?,
                name:        r.get(1)?,
                color:       r.get(2)?,
                created_at:  r.get(3)?,
                sound_count: r.get(4)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    Json(projects)
}

async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateProjectBody>,
) -> Result<Json<Project>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO projects (name, color, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![body.name, body.color, now],
    )
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let id = conn.last_insert_rowid();
    Ok(Json(Project {
        id,
        name: body.name,
        color: body.color,
        sound_count: 0,
        created_at: now,
    }))
}

async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    let n = conn
        .execute("DELETE FROM projects WHERE id = ?", [id])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if n == 0 {
        return Err((axum::http::StatusCode::NOT_FOUND, "project not found".to_string()));
    }
    Ok(Json(serde_json::json!({"deleted": true})))
}

async fn get_project_sounds(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Json<Vec<Sound>> {
    let sql = format!(
        "SELECT {SOUND_COLS} FROM sounds s \
         JOIN project_sounds ps ON ps.sound_id = s.id \
         WHERE ps.project_id = ? ORDER BY ps.added_at DESC"
    );

    let (mut sounds, tags_map, embed_set) = {
        let conn = state.pool.get();
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Json(vec![]),
        };
        let sounds: Vec<Sound> = stmt
            .query_map([id], row_to_sound)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default();
        let ids: Vec<i64> = sounds.iter().map(|s| s.id).collect();
        let tags_map  = fetch_tags_for_ids(&conn, &ids);
        let embed_set = fetch_embedding_flags(&conn, &ids);
        (sounds, tags_map, embed_set)
    };

    for sound in &mut sounds {
        sound.tags          = tags_map.get(&sound.id).cloned().unwrap_or_default();
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

    Json(sounds)
}

async fn add_sounds_to_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<AddSoundsBody>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    // Verify project exists
    let exists: bool = conn
        .query_row("SELECT COUNT(*) FROM projects WHERE id = ?", [id], |r| r.get::<_, i64>(0))
        .unwrap_or(0) > 0;
    if !exists {
        return Err((axum::http::StatusCode::NOT_FOUND, "project not found".to_string()));
    }
    let now = chrono::Utc::now().to_rfc3339();
    for sound_id in &body.sound_ids {
        conn.execute(
            "INSERT OR IGNORE INTO project_sounds (project_id, sound_id, added_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, sound_id, now],
        )
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(serde_json::json!({"added": body.sound_ids.len()})))
}

async fn remove_sound_from_project(
    State(state): State<Arc<AppState>>,
    Path((project_id, sound_id)): Path<(i64, i64)>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let conn = state.pool.get();
    conn.execute(
        "DELETE FROM project_sounds WHERE project_id = ?1 AND sound_id = ?2",
        rusqlite::params![project_id, sound_id],
    )
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({"removed": true})))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/projects",                              get(list_projects).post(create_project))
        .route("/api/projects/:id",                          delete(delete_project))
        .route("/api/projects/:id/sounds",                   get(get_project_sounds).post(add_sounds_to_project))
        .route("/api/projects/:id/sounds/:sound_id",         delete(remove_sound_from_project))
}
