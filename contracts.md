# Daouchi HTTP API contract

The Rust core exposes a JSON-over-HTTP API that the web UI calls. It is served in-process through the desktop shell's custom URI scheme; there is no network listener. Paths below are relative to the UI origin (`daouchi://localhost/`, or `http://daouchi.localhost/` on Windows). `schema.sql` is the schema reference for the objects returned here.

---

## Transport

- **Origin and dispatch.** The webview loads the UI from the scheme origin. Each request is converted to an axum request (method, path, query, headers, body) and run through the router in the same process. There is no CORS layer; cross-origin requests receive no `access-control-*` headers.
- **No streaming.** Custom-scheme responses are delivered whole, so Server-Sent Events do not work in the desktop webview. The SSE routes exist (`/api/index/progress`, `/api/models/events`) but the UI polls the equivalent status endpoints instead: `GET /api/index/status` (about every 500 ms) and `GET /api/models` (every 1 s while a download runs, otherwise every 4 s).
- **Range requests.** `GET /audio/stream` honours `Range: bytes=START-END` and `bytes=START-` with `206 Partial Content`, `Content-Range`, `Content-Length` and `Accept-Ranges: bytes`. A single range is clamped to 16 MiB; clients continue with follow-up ranges. A malformed or unsatisfiable range returns `416`.
- **64 MiB cap.** A response with a known `Content-Length` above 64 MiB to a plain `GET` (no `Range` header) is cut to its first 64 MiB and returned as `206` with `Content-Range: bytes 0-67108863/<total>` and `Accept-Ranges: bytes`. Responses to requests that carry `Range`, and `HEAD` responses, are never altered.
- **Caching.** Every response carries `Cache-Control: no-cache, no-store, must-revalidate`, except `/api/sounds/:id/artwork`, which is `max-age=31536000, immutable`.

### Errors

| Status | Body | Meaning |
|---|---|---|
| 400 | plain text | Bad request or unknown model name |
| 403 | plain text | Path outside indexed roots / exports directory |
| 404 | plain text | Sound, project, bundle or file not found |
| 409 | `{ "error": "model_not_ready", "model": "clap" \| "ast" }` | A required AI model is missing or incomplete |
| 409 | `{ "error": "already_running" \| "not_running", "message": "..." }` | Indexing state conflict |
| 409 | plain text | Rename destination exists |
| 416 | `bytes */<size>` | Unsatisfiable `Range` |
| 500 | plain text | Internal error |
| 503 | `{ "error": "engine_unavailable", "detail": "..." }` | The engine could not start (for example, the ONNX Runtime library is missing) |

Handlers that need an AI model: `POST /api/sounds/semantic_search`, `POST /api/ai/mood_search`, `POST /api/ai/sfx_search`, `GET /api/ai/sfx_category_counts` (when embeddings exist), `POST /api/radar/generate` and `POST /api/radar/compute`. Auto-tagging (`GET /api/sounds/:id/suggested_tags`) runs in the background and does not return these errors; see Suggested tags.

---

## Indexing

```
POST   /api/index/start
       Request:  { "folders": [ { "path": "/abs/path", "source": "sfx" | "music" } ] }
       Response: 200 { "status": "started" }
                 409 { "error": "already_running", ... } while a run is in flight

GET    /api/index/status       Latest snapshot, or { "done": 0, "total": 0, "status": "idle", "phase": "idle", "can_cancel": false }
GET    /api/index/progress     SSE; same snapshot JSON per event. Replays the latest snapshot on connect, closes after a terminal one.
POST   /api/index/cancel       202 { "status": "cancelling" } | 409 { "error": "not_running", ... }
```

Snapshot (the SSE data and the status body are the same object):

```
{ "phase": "scanning" | "reading" | "writing" | "finalizing" | "done" | "cancelled" | "error",
  "status": "indexing" | "done" | "cancelled" | "error: <msg>",
  "done": 120, "total": 6160,                 // total is 0 until total_known
  "total_known": false, "discovered": 4210,   // audio files found by the scan so far
  "added": 118,                               // rows written
  "current_root": "/path", "current_path": "pack1/boom.wav",   // relative, tail-truncated
  "roots_done": 1, "roots_total": 3,
  "skipped": 2,                               // unreadable files, not added
  "errors": 5,
  "recent_errors": [ { "path": "pack1/x.wav", "reason": "unsupported or corrupt audio" } ],   // last 20
  "bytes_done": 123456, "started_at_ms": 1760000000000, "elapsed_ms": 4200,
  "files_per_sec": 83.4, "eta_ms": 12000 | null,
  "can_cancel": true, "message": null }
```

Phases run `scanning`, then `reading` and `writing` per batch of 100 files, then `finalizing`, then a terminal phase (`done`, `cancelled` or `error`). The terminal snapshot is always published.

Safety and cancel semantics: indexing is additive. Files are upserted by `path` (the row id is preserved) in one transaction per 100-file batch. Nothing is deleted, including rows for files that vanished from disk, and tags, favorites, ratings, embeddings, suggested tags, radar coordinates and projects are never modified. Cancel is cooperative: it takes effect at the next batch boundary (immediately while scanning); committed batches are kept and the rest is not processed. Roots with at least one processed file are registered in `indexed_roots` in the finalizing step (one transaction). Quitting or crashing mid-run behaves like a cancel (a partial batch rolls back). Corrupt or unsupported audio is not fatal: the file is added without duration and format details and listed in `recent_errors`. Supported extensions: `wav`, `mp3`, `m4a`, `flac`, `aiff`, `aif`.

---

## Sounds and search

```
GET    /api/sounds/search
       Query: q, source (sfx|music), ext, dur_min, dur_max (seconds), favorite (bool), tag (repeatable),
              bundle_id, folder (collection or folder_path prefix), label (0-5), min_rating (1-5),
              sort, limit, offset
       sort:  date_desc (default) | date_asc | name_asc | name_desc | duration_asc | duration_desc |
              size_desc | modified_desc | modified_asc
       Response: { "results": [Sound], "total": N }
       q is matched with FTS5 prefix semantics on the last token.

GET    /api/sounds/:id                 Sound | 404
GET    /api/sounds/recent?limit=       { "results": [Sound], "total": N }          (default 10, max 500)
GET    /api/sounds/stats               { "total", "total_duration", "sfx_count", "music_count", "with_embeddings",
                                         "with_stems", "tag_count",
                                         "collections": [{ "name", "count", "avg_duration", "source" }],
                                         "top_tags": [{ "tag", "count" }] }
GET    /api/sounds/similar/:id?limit=  { "results": [Sound], "total": N }          nearest neighbours by embedding (default 30, max 200)
POST   /api/sounds/semantic_search     Request { "query": "subtle rain", "limit"?: 50 } -> { "results": [Sound], "total": N }, best match first
GET    /api/sounds/:id/transients      { "sound_id": N, "fractions": [0.0 .. 1.0] }  onset positions as fractions of duration (at most 80)
GET    /api/sounds/:id/artwork         image/jpeg | image/png (embedded cover art) or a generated image/svg+xml; cached in <home>/cache/artwork
```

## Sound metadata

```
POST   /api/sounds/:id/favorite        -> { "favorite": true | false }            toggles
POST   /api/sounds/:id/rating          { "rating": 0..5 }  -> { "rating": N }     clamped; 0 = unrated
POST   /api/sounds/:id/label           { "label": 0..5 }   -> { "label": N }      clamped; 0 = none
POST   /api/sounds/:id/tags            { "add": [".."], "remove": [".."] } -> { "tags": [".."] }
POST   /api/sounds/bulk_tags           { "sound_ids": [N], "add": [".."], "remove": [".."] } -> { "updated": N }
GET    /api/tags                       -> { "tags": [".."] }
GET    /api/folders                    -> { "tree": [FolderNode] }
                                       FolderNode: { key, name, path (search filter key), source, count, root_id|null, root_path|null, rel_path (relative to root),
                                       direct_only, abs_path (display only), children }. Actions must send root_id + rel_path, never `path`.
POST   /api/sounds/:id/rename          { "new_name"?: "x.wav", "new_path"?: "/abs/dest.wav" } -> { "ok": true, "id", "path", "filename", "folder_path" }
                                       403 if the destination is outside indexed roots, 409 if it exists
POST   /api/sounds/:id/delete          { "trash"?: true } -> { "deleted": true, "id": N, "trashed": bool }
                                       Always moves the file to the Trash (never a permanent delete; `trash` is ignored). Same code path as
                                       /api/library/remove with delete_files=true. 500 and the row is kept if the Trash move fails.
POST   /api/reveal                     { "kind": "sound", "id": N } | { "kind": "folder", "root_id": N, "rel_path": "Coll/sub" }
                                       -> { "ok": true, "path": "/abs/resolved" }   reveals in Finder
                                       The server resolves root + relative path from `indexed_roots`, canonicalizes and requires the result to exist
                                       inside the root. 400 for `..`/absolute rel_path, 404 if missing/root unavailable, 403 for a symlink escape.
                                       Legacy { "path": "/abs" } is still accepted but must lie inside an indexed root.
GET    /api/library/roots              -> { "roots": [{ "id", "path", "source", "sounds", "exists" }] }
POST   /api/library/remove_preview     { "kind": "folder", "root_id": N, "rel_path": "" , "direct_only"?: false } | { "kind": "sound", "id": N }
                                       -> { "path": "/abs", "sounds": N, "bytes": N, "is_root": bool, "exists": bool }     no side effects
                                       rel_path "" = the indexed root itself (is_root true); direct_only limits to files directly inside (the "Unsorted" node).
POST   /api/library/remove             { ...same target..., "delete_files": false }
                                       -> { "removed_sounds": N, "trashed_files": N, "failed": [{ "path", "reason" }], "removed_root": "/path" | null }
                                       Removes sounds and dependents (tags, suggested_tags, embeddings, radar_coords, play_history, project_sounds, FTS) in one
                                       transaction. delete_files=true first moves each indexed audio file to the Trash (re-validated against its root;
                                       symlink escapes refused); only successfully trashed (or already missing) files lose their row, failures are listed.
                                       Emptied directories that held trashed files are removed bottom-up; the root directory is never removed or trashed.
                                       Removing a whole root also deletes its `indexed_roots` row (only when nothing failed).
```

### Suggested tags

```
GET    /api/sounds/:id/suggested_tags            -> { "suggestions": [{ "tag": "..", "score": 0.92 }] }
                                                 Returns the stored suggestions. If there are none, AST tagging is started in the
                                                 background and the list is empty until it finishes (poll again). Requires the `ast`
                                                 model; `clap` is used for cross-validation when ready.
DELETE /api/sounds/:id/suggested_tags            -> { "cleared": true }       clears the cache so the next GET re-runs the model
POST   /api/sounds/:id/suggested_tags/:tag/confirm   -> { "tag": "..", "confirmed": true }      promotes to a real, searchable tag
DELETE /api/sounds/:id/suggested_tags/:tag       -> { "dismissed": true }
```

### Tag bundles

```
POST   /api/bundles                    { "name": "horror-2025" } -> Bundle
GET    /api/bundles                    -> { "bundles": [Bundle] }
POST   /api/bundles/:id/tags           { "add": [".."], "remove": [".."] } -> { "tags": [".."] }      404 for an unknown bundle
DELETE /api/bundles/:id                -> { "deleted": true }
```

### Projects

```
GET    /api/projects                   -> [Project]
POST   /api/projects                   { "name": "..", "color"?: "#e47a2a" } -> Project
DELETE /api/projects/:id               -> { "deleted": true }
GET    /api/projects/:id/sounds        -> [Sound]
POST   /api/projects/:id/sounds        { "sound_ids": [N] } -> { "added": N }
DELETE /api/projects/:id/sounds/:sound_id   -> { "removed": true }
```

---

## AI features

### Mood and SFX categories

```
POST   /api/ai/mood_search             { "valence": -1..1, "arousal": -1..1, "limit"?: 30 (max 100) }
                                       -> { "results": [Sound + "mood_score"], "query": "<text prompt>", "total": N }
GET    /api/ai/sfx_categories          -> { "categories": [{ "id", "label", "emoji", "query" }] }       ids: riser, impact, atmosphere, foley, whoosh
GET    /api/ai/sfx_category_counts     -> { "counts": { "<id>": N, ... }, "total_embedded": N }     counts above the similarity threshold
POST   /api/ai/sfx_search              { "category": "riser"|"impact"|"atmosphere"|"foley"|"whoosh", "limit"?: 30 (max 100) }
                                       -> { "results": [Sound], "total": N }
```

### Models

```
GET    /api/models
       -> [{ "name": "clap"|"ast", "label": "..", "total_bytes": N, "status": "missing"|"partial"|"ready",
             "downloading": null | { "done_bytes": N, "total_bytes": N, "file": ".." }, "error"?: "last download error" }]

POST   /api/models/:name/download      idempotent; starts a background download and verification. 202 + the array above. 400 for an unknown name.
GET    /api/models/events              SSE; first event on connect, then one per change (progress throttled to about 4/s). Data = the array above.
GET    /api/bootstrap/status           -> { "status": "downloading" | "done",
                                            "models": [{ "id", "name", "downloaded_mb", "total_mb", "progress": 0..1 }] }
                                       "done" whenever no download is running (models are optional).
```

### Acoustic radar

Only embeddings of the current model are used (see Embedding model versioning in `docs/ARCHITECTURE.md`).

```
GET    /api/radar/status
       -> { "status": "none" | "not_started" | "computing" | "ready" | "error",
            "progress": { "done": N, "total": N },     // done = embedded with the current model, total = all sounds
            "phase": "" | "embedding" | "umap", "embedded": N, "mapped": N, "error"?: "..",
            "job": { "state": "idle"|"running"|"done"|"error", "phase", "done", "total", "failed", "progress": 0..1, "error": null | ".." } }

       none         library is empty
       not_started  sounds exist but no radar coordinates yet
       computing    a background job is running
       ready        coordinates exist
       error        the last job failed (see error)

GET    /api/radar/points               -> { "points": [{ "sound_id", "x", "y", "filename", "duration", "source", "collection" }] }   at most 10000
POST   /api/radar/generate             embed every sound lacking a current-model embedding, then run UMAP (background). -> { "status": "started" }
POST   /api/radar/compute              UMAP only, over existing embeddings (background). -> { "status": "started" }
```

Both POST routes return `409 model_not_ready` if CLAP is missing. Job state is persisted in `ai_jobs`, and a job always ends in `done` or `error`.

---

## Export

```
POST   /api/sounds/:id/export
       Request:  { "start_ms"?: N, "end_ms"?: N, "fx_preset"?: null | "none" | "underwater" | "telephone" | "clubdrop" | "lofi",
                   "format"?: "wav" }
       Response: { "path": "/abs/exports/name.wav", "filename": "name.wav", "duration_ms": N, "sample_rate": N, "bits": 16 | 24 | 32 }
       Errors:   400 (bad bounds, unknown fx_preset, unsupported format), 404 (sound missing)
       Notes:    sample-exact cut of [start_ms, end_ms); omitted start = 0, omitted end = end of file (clamped to the file length).
                 Source sample rate and channel count are preserved. Output is 24-bit PCM for sources of 24 bits or more,
                 32-bit float for float sources, otherwise 16-bit PCM.
                 Written to <home>/exports/<name>_<start>-<end>[_<fx>].wav; an existing name gets a numeric suffix.

POST   /api/export/reveal
       Request:  { "path": "/abs/exports/name.wav" }
       Response: { "success": true } | 403 if the path is not a file inside the exports directory
```

Dragging an exported file into another application is a native desktop-shell feature, not an HTTP route.

---

## Audio streaming

```
GET    /audio/stream?path=/absolute/path/file.wav
       Headers:  Range: bytes=0-65535 (optional)
       200 full body | 206 audio bytes (see Transport) | 403 outside indexed roots | 404 | 416
```

The path is canonicalized and must lie inside a registered indexed root (or be an indexed file). `Content-Type` is derived from the extension.

---

## Stems (not available)

Stem separation is not part of this version. The routes exist and answer consistently:

```
GET    /api/stems/:id          -> { "status": "not_available", "reason": "stems not included in this build", "stems": null }
POST   /api/stems/:id/split    -> { "status": "not_available", "reason": "stems not included in this build" }
POST   /api/stems/:id/reveal   -> { "path": "<stems cache dir>" }
POST   /api/stems/:id/save     copies stem WAVs that already exist in the stems cache next to the source file
```

If all four stem WAVs already exist in the cache, `GET` returns `{ "status": "done", "stems": { "drums", "bass", "vocals", "other" } }`.

---

## Static UI

Any other `GET` is served from the embedded `web/` directory (or `DAOUCHI_WEB_DIR` when set).

---

## Object shapes

### Sound

```json
{
  "id": 1,
  "path": "/Volumes/Audio/sfx/collection/sound.wav",
  "filename": "sound.wav",
  "extension": "wav",
  "size": 204800,
  "duration": 2.34,
  "bitrate": 1411,
  "sample_rate": 44100,
  "bit_depth": 16,
  "collection": "99Sounds Cinematic",
  "folder_path": "99Sounds Cinematic/Whooshes",
  "source": "sfx",
  "favorite": false,
  "tags": ["whoosh", "transition"],
  "has_embedding": true,
  "has_stems": false,
  "indexed_at": "2026-06-30T10:00:00Z",
  "file_modified_at": "2026-05-01T08:00:00Z",
  "rating": 0,
  "label": 0
}
```

### Project

```json
{ "id": 1, "name": "Trailer", "color": "#e47a2a", "sound_count": 12, "created_at": "2026-06-30T10:00:00Z" }
```

### Bundle

```json
{ "id": 1, "name": "horror-2025", "tags": ["stab", "impact", "drone"], "created_at": "2026-06-30T10:00:00Z" }
```

### FolderNode

```json
{ "name": "Whooshes", "path": "99Sounds Cinematic/Whooshes", "count": 42, "children": [] }
```

---

## Concurrency

One process, one `rusqlite` connection behind a mutex, journal mode DELETE (safe on exFAT). The mutex guard is never held across inference or an `.await`. Inference runs on blocking threads.
