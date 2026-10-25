/// Emotion Matrix — Russell Circumplex mood search.
///
/// POST /api/ai/mood_search
/// Body: { "valence": -1.0..1.0, "arousal": -1.0..1.0, "limit": 30 }
///
/// Translates a 2D arousal/valence coordinate into a text prompt and
/// runs it through CLAP semantic search. Returns the same Sound list
/// format as /api/sounds/semantic_search.
///
/// Quadrant → text prompt mapping:
///   High arousal  + Negative valence → "heavy aggressive dark intense sound design"
///   High arousal  + Positive valence → "energetic exciting powerful dramatic music"
///   Low arousal   + Negative valence → "dark melancholic sad ambient drone atmosphere"
///   Low arousal   + Positive valence → "calm peaceful gentle soothing ambient relaxing"
///   Near-center blends are interpolated from the quadrant prompts.
use crate::api::search::{Sound, SOUND_COLS};
use crate::api::{ApiError, AppState};
use axum::{extract::State, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
pub struct MoodRequest {
    /// X axis: -1.0 = maximally negative valence, +1.0 = maximally positive
    #[serde(default)]
    pub valence: f64,
    /// Y axis: -1.0 = maximally low arousal (calm), +1.0 = maximally high arousal (intense)
    #[serde(default)]
    pub arousal: f64,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize { 30 }

#[derive(Serialize)]
pub struct MoodResponse {
    pub results: Vec<MoodResult>,
    pub query: String,
    pub total: i64,
}

#[derive(Serialize)]
pub struct MoodResult {
    #[serde(flatten)]
    pub sound: Sound,
    pub mood_score: f64,
}

/// Translate a (valence, arousal) point in [-1, 1]² to a CLAP text query.
/// Uses bilinear interpolation between the four quadrant archetypes so that
/// edge points produce stronger (more specific) prompts and center produces
/// a neutral "ambient texture" query.
pub fn coords_to_prompt(valence: f64, arousal: f64) -> String {
    // Clamp to [-1, 1]
    let v = valence.clamp(-1.0, 1.0);
    let a = arousal.clamp(-1.0, 1.0);

    // Quadrant weights (bilinear)
    let w_tense      = ((-v + 1.0) / 2.0) * ((a + 1.0) / 2.0);  // high arousal, neg valence
    let w_aggressive = ((v + 1.0) / 2.0)  * ((a + 1.0) / 2.0);  // high arousal, pos valence (→ energetic/action)
    let w_melancholy = ((-v + 1.0) / 2.0) * ((-a + 1.0) / 2.0); // low arousal, neg valence
    let w_calm       = ((v + 1.0) / 2.0)  * ((-a + 1.0) / 2.0); // low arousal, pos valence

    // Dominant quadrant determines the base prompt; weighted blends add flavour
    let quadrants = [
        (w_tense,      "tense suspenseful dark brooding cinematic tension metallic friction"),
        (w_aggressive, "energetic powerful aggressive explosive impactful driving force"),
        (w_melancholy, "sad melancholic sorrowful haunting mournful somber ambient drone"),
        (w_calm,       "calm peaceful serene gentle soothing tranquil ambient nature relaxing"),
    ];

    // Pick dominant quadrant prompt and prepend intensity modifier
    let (_, dominant_prompt) = quadrants
        .iter()
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .copied()
        .unwrap_or((1.0, "ambient sound"));

    // Intensity: how far from center (0,0)
    let intensity = (v * v + a * a).sqrt() / std::f64::consts::SQRT_2;

    let prefix = if intensity > 0.7 {
        "very "
    } else if intensity > 0.3 {
        ""
    } else {
        "subtly "
    };

    // The CLAP 630k model was trained with "this is a sound of " prefix
    format!("this is a sound of {prefix}{dominant_prompt}")
}

/// SFX sub-category prompts for the Sonic Distribution Blueprint.
/// Each variant maps to a CLAP text query used to score/classify sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SfxCategory {
    Riser,
    Impact,
    Atmosphere,
    Foley,
    Whoosh,
}

impl SfxCategory {
    pub fn clap_query(self) -> &'static str {
        match self {
            SfxCategory::Riser     => "this is a sound of riser sweep ascending pitch transition buildup",
            SfxCategory::Impact    => "this is a sound of impact hit boom explosion cinematic sub thud",
            SfxCategory::Atmosphere => "this is a sound of atmospheric drone ambient texture room tone environmental background",
            SfxCategory::Foley     => "this is a sound of foley footstep organic everyday sound real world",
            SfxCategory::Whoosh    => "this is a sound of whoosh swoosh fast motion air movement transition",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SfxCategory::Riser      => "Risers / Sweeps",
            SfxCategory::Impact     => "Impacts / Hits",
            SfxCategory::Atmosphere => "Atmospheres / Drones",
            SfxCategory::Foley      => "Foley",
            SfxCategory::Whoosh     => "Whooshes / Swooshes",
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            SfxCategory::Riser      => "📈",
            SfxCategory::Impact     => "💥",
            SfxCategory::Atmosphere => "🍃",
            SfxCategory::Foley      => "👞",
            SfxCategory::Whoosh     => "💨",
        }
    }

    pub fn all() -> &'static [SfxCategory] {
        &[
            SfxCategory::Riser,
            SfxCategory::Impact,
            SfxCategory::Atmosphere,
            SfxCategory::Foley,
            SfxCategory::Whoosh,
        ]
    }
}

pub async fn mood_search(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MoodRequest>,
) -> Result<Json<MoodResponse>, ApiError> {
    let query = coords_to_prompt(body.valence, body.arousal);
    let limit = body.limit.min(100);

    let semantic_results = state
        .ai
        .semantic_search(&query, limit)
        .await?;

    if semantic_results.is_empty() {
        return Ok(Json(MoodResponse { results: vec![], query, total: 0 }));
    }

    let ids: Vec<i64> = semantic_results.iter().map(|r| r.sound_id).collect();
    let score_map: std::collections::HashMap<i64, f64> =
        semantic_results.iter().map(|r| (r.sound_id, r.score)).collect();

    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT {SOUND_COLS} FROM sounds s WHERE s.id IN ({placeholders})");

    use crate::api::search::row_to_sound;
    use crate::api::search::fetch_tags_for_ids;
    use crate::api::search::fetch_embedding_flags;
    use crate::api::search::has_stems_on_disk;

    let (mut sounds, tags_map, embed_set) = {
        let conn = state.pool.get();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let sounds: Vec<Sound> = stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), row_to_sound)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default();
        let sound_ids: Vec<i64> = sounds.iter().map(|s| s.id).collect();
        let tags_map = fetch_tags_for_ids(&conn, &sound_ids);
        let embed_set = fetch_embedding_flags(&conn, &sound_ids);
        (sounds, tags_map, embed_set)
    };

    for s in &mut sounds {
        s.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
        s.has_embedding = embed_set.contains(&s.id);
    }

    let mut sounds = tokio::task::spawn_blocking(move || {
        for s in &mut sounds {
            s.has_stems = has_stems_on_disk(&s.path);
        }
        sounds
    })
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Sort by score descending
    sounds.sort_by(|a, b| {
        score_map
            .get(&b.id)
            .unwrap_or(&0.0)
            .partial_cmp(score_map.get(&a.id).unwrap_or(&0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total = sounds.len() as i64;
    let results = sounds
        .into_iter()
        .map(|s| {
            let mood_score = *score_map.get(&s.id).unwrap_or(&0.0);
            MoodResult { sound: s, mood_score }
        })
        .collect();

    Ok(Json(MoodResponse { results, query, total }))
}

/// GET /api/ai/sfx_categories
/// Returns the list of SFX sub-categories with their labels and emoji for the Blueprint UI.
/// No AI inference — just the static catalogue.
async fn sfx_category_list() -> Json<serde_json::Value> {
    let categories: Vec<_> = SfxCategory::all()
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": format!("{:?}", c).to_lowercase(),
                "label": c.label(),
                "emoji": c.emoji(),
                "query": c.clap_query(),
            })
        })
        .collect();
    Json(serde_json::json!({ "categories": categories }))
}

/// POST /api/ai/sfx_search
/// Body: { "category": "riser" | "impact" | "atmosphere" | "foley" | "whoosh", "limit": 30 }
/// Runs a CLAP semantic search using the category's canonical prompt.
#[derive(Deserialize)]
struct SfxSearchBody {
    category: SfxCategory,
    #[serde(default = "default_limit")]
    limit: usize,
}

async fn sfx_category_search(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SfxSearchBody>,
) -> Result<Json<crate::api::search::SearchResponse>, ApiError> {
    let query = body.category.clap_query();
    let limit = body.limit.min(100);

    let semantic_results = state
        .ai
        .semantic_search(query, limit)
        .await?;

    if semantic_results.is_empty() {
        return Ok(Json(crate::api::search::SearchResponse { results: vec![], total: 0 }));
    }

    let ids: Vec<i64> = semantic_results.iter().map(|r| r.sound_id).collect();
    let score_map: std::collections::HashMap<i64, f64> =
        semantic_results.iter().map(|r| (r.sound_id, r.score)).collect();

    use crate::api::search::row_to_sound;
    use crate::api::search::fetch_tags_for_ids;
    use crate::api::search::fetch_embedding_flags;
    use crate::api::search::has_stems_on_disk;

    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT {SOUND_COLS} FROM sounds s WHERE s.id IN ({placeholders})");

    let (mut sounds, tags_map, embed_set) = {
        let conn = state.pool.get();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let sounds: Vec<Sound> = stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), row_to_sound)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default();
        let sids: Vec<i64> = sounds.iter().map(|s| s.id).collect();
        let tags_map = fetch_tags_for_ids(&conn, &sids);
        let embed_set = fetch_embedding_flags(&conn, &sids);
        (sounds, tags_map, embed_set)
    };

    for s in &mut sounds {
        s.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
        s.has_embedding = embed_set.contains(&s.id);
    }

    let mut sounds = tokio::task::spawn_blocking(move || {
        for s in &mut sounds {
            s.has_stems = has_stems_on_disk(&s.path);
        }
        sounds
    })
    .await
    .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    sounds.sort_by(|a, b| {
        score_map
            .get(&b.id)
            .unwrap_or(&0.0)
            .partial_cmp(score_map.get(&a.id).unwrap_or(&0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total = sounds.len() as i64;
    Ok(Json(crate::api::search::SearchResponse { results: sounds, total }))
}

/// GET /api/ai/sfx_category_counts
/// Computed in-process. Returns per-category counts of matching sounds.
async fn sfx_category_counts_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let data = state
        .ai
        .sfx_category_counts()
        .await?;
    Ok(Json(data))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/ai/mood_search", post(mood_search))
        .route("/api/ai/sfx_categories", get(sfx_category_list))
        .route("/api/ai/sfx_category_counts", get(sfx_category_counts_handler))
        .route("/api/ai/sfx_search", post(sfx_category_search))
}
