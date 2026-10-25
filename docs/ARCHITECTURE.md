# Architecture

Daouchi is one process: a Tauri desktop shell hosting an axum `Router`, a SQLite database and in-process ONNX Runtime engines. This document records the design decisions and the numeric specifications the implementation follows. The HTTP surface is specified in [../contracts.md](../contracts.md).

## Components

| Component | Location | Role |
|---|---|---|
| Core library `daouchi` | `src/` | `build_app()` assembles storage, database, engines and the `Router`; indexer; request transport |
| AI crate `daouchi-ai` | `crates/daouchi-ai/` | Model manager, ONNX Runtime bootstrap, CLAP, AST, Kaldi fbank, UMAP, transient detection, audio decode |
| Desktop shell `daouchi-desktop` | `crates/daouchi-desktop/` | Tauri v2 window, `daouchi` URI scheme handler, drag-out plugin, single-instance guard |
| Web UI | `web/` | Alpine.js application embedded into the binary (rust-embed); libraries vendored in `web/vendor/` |

`daouchi::build_app()` binds nothing. It resolves the home directory, opens and migrates the database, creates the engine registry (no model is required to start) and returns the `Router`.

## Transport

The webview loads `daouchi://localhost/` (macOS and Linux) or `http://daouchi.localhost/` (Windows). The shell registers an asynchronous Tauri URI scheme; each request is converted by `daouchi::transport` to an axum request (method, path and query, headers, body) and executed with `tower::ServiceExt::oneshot`. The response is collected and handed back to the webview.

Consequences:

- **No network port.** No socket is bound, so no other process or web page can reach the API. The router has no CORS layer; a request with a foreign `Origin` receives no `access-control-*` headers.
- **Same origin.** Tauri treats both scheme origins as local, so `window.__TAURI__` and the drag plugin are available. `capabilities/default.json` lists exactly those two origins. `app.security.csp` is `null`; a CSP would need the scheme origins in `default-src`, `connect-src`, `media-src`, `img-src` and `script-src`, plus `blob:` for media and connect (WaveSurfer plays a blob URL).
- **Whole-body responses.** A scheme response cannot stream, so Server-Sent Events do not work. `web/components/transport.js` replaces `EventSource` in the desktop webview with polling (`/api/models/events` becomes `GET /api/models`; `/api/index/progress` becomes `GET /api/index/status`). The SSE routes remain for non-desktop callers.
- **Range semantics.** `/audio/stream` answers `Range` requests with `206` and clamps one range to 16 MiB. WaveSurfer v7 `load(url)` fetches the whole file without `Range`. To keep memory bounded, a non-Range `200` whose declared `Content-Length` exceeds 64 MiB (`transport::MAX_BODY`) is cut to its first 64 MiB and returned as `206` with `Content-Range: bytes 0-67108863/<total>` and `Accept-Ranges: bytes`. For such files the waveform is therefore decoded from the first 64 MiB only; media elements pointed at the URL use ordinary Range requests. `HEAD` keeps headers and sends no body.
- **Logging.** `RUST_LOG=daouchi=debug` logs each scheme request and response.

## Storage

Everything the application creates lives under `DAOUCHI_HOME` (default `~/.daouchi`), resolved by `daouchi_ai::home::DaouchiHome`:

```
data/sounds.db       SQLite (journal mode DELETE, foreign keys on, 5 s busy timeout; safe on exFAT)
models/<name>/       model files + manifest.json
cache/artwork/       generated or extracted cover images
cache/coreml/        CoreML compiled-model cache
exports/             exported clips
lib/                 optional ONNX Runtime library location
```

Nothing ephemeral is written into the repository or next to the audio files. Source audio is only ever read, except for the explicit rename and delete actions a user triggers in the UI.

### Database concurrency

One `rusqlite` connection behind a mutex (`DbPool`). The guard is always scoped to a block that ends before any `.await`, so futures stay `Send`, and it is never held across inference. Schema changes are versioned with `PRAGMA user_version` in `db_migrate.rs` and are additive; user tables are never dropped.

### Models and manifests

`models/<name>/manifest.json` lists `name`, `version` and `files[]` (`path`, `url`, `sha256`, `size`). A manifest on disk wins over the built-in one, which is what makes preseeding work. A model is `ready` only when every file exists with the right size and sha256; `partial` when some do; `missing` otherwise. Downloads are resumable and sha256-verified, run as background tasks, and publish progress that the UI polls. Built-in manifests pin Hugging Face revisions:

- `clap`: `Xenova/clap-htsat-unfused` (`onnx/audio_model.onnx`, `onnx/text_model.onnx`, tokenizer and config files)
- `ast`: `Xenova/ast-finetuned-audioset-10-10-0.4593` (`onnx/model.onnx`, config files)

## Runtime

ONNX Runtime is used through the `ort` crate (`=2.0.0-rc.10`, `load-dynamic`), which targets ONNX Runtime API 22, so a 1.22.x shared library is required. The library is located in this order: `ORT_DYLIB_PATH`, `<exe dir>/../Frameworks`, `<exe dir>`, `<home>/lib`. If it cannot be found, the application starts and engines report `engine_unavailable` (HTTP 503).

Session options on macOS:

- CoreML execution provider, model format `MLProgram`, compute units `All`, compiled-model cache in `<home>/cache/coreml`.
- Static input shapes: every free (symbolic) input dimension, such as the batch size, is overridden to 1, because otherwise CoreML takes only a fraction of the graph. Callers therefore feed batch size 1.
- Any CoreML failure is logged and the session falls back to CPU (graph optimization level 3). The first CoreML compile of a model takes about 2 minutes and is cached.

Engine rules: inference runs in `tokio::task::spawn_blocking`; a missing model yields `ModelNotReady` (409) rather than a hang; engines load lazily on first use; background jobs (radar generate, UMAP) have persisted state in `ai_jobs` and always end in `done` or `error`. At exit the ONNX Runtime environment is released from an `atexit` handler so the library's own static destructors do not abort.

## CLAP (semantic search, mood, SFX categories, radar)

Checkpoint: `Xenova/clap-htsat-unfused`, ONNX; 512-dimensional embeddings.

**Audio branch** (equivalent to the Hugging Face `ClapFeatureExtractor` with `rand_trunc` / `repeatpad`):

1. Decode, resample to 48 kHz mono.
2. int16 round trip: `clip(-1, 1)`, truncate `x * 32767` to integer, divide by 32767.
3. Fit to 480000 samples (10 s). Shorter input is tiled `floor(480000 / len)` times and zero-padded; longer input is center-cropped (default) or processed as the mean over non-overlapping 10 s chunks, then L2-normalized.
4. STFT: `n_fft` 1024, hop 480, periodic Hann window, centered with reflect padding, power spectrum.
5. 64-band Slaney mel, `fmin` 50, `fmax` 14000, Slaney normalization (reference matrix `crates/daouchi-ai/assets/melW_513x64_f32le.bin`).
6. `10 * log10(max(x, 1e-10))`.
7. Model input `[1, 1, 1001, 64]`; output `audio_embeds` is not normalized, so it is L2-normalized afterwards.

**Text branch:** prefix `"this is a sound of "`, RoBERTa `tokenizer.json`, padded and truncated to 77 tokens, `input_ids` as int64 only (no attention mask); output `text_embeds`, L2-normalized.

**Ranking:** cosine similarity of the query vector against all stored vectors of the current model.

**SFX categories:** five fixed prompts (riser, impact, atmosphere, foley, whoosh). A sound counts for a category when its cosine to the prompt is at least `SFX_THRESHOLD = 0.22`.

**Mood search:** the valence/arousal point (each in [-1, 1]) is mapped by bilinear weighting between four quadrant prompts to a text prompt, which is then run as a semantic search.

## AST (auto-tag suggestions)

Checkpoint: `Xenova/ast-finetuned-audioset-10-10-0.4593`, ONNX; 527 AudioSet classes.

**Kaldi-style fbank** (pure Rust, numerically matched to the Hugging Face `ASTFeatureExtractor`):

1. Mono f32 at 16 kHz in [-1, 1] (no `2^15` scaling).
2. Frames of 400 samples, shift 160, snip edges.
3. Subtract the frame mean, pre-emphasis 0.97 (`x[-1] = x[0]`), symmetric Hann `0.5 - 0.5 cos(2 pi k / 399)`.
4. Zero-pad to 512, real FFT, power spectrum, drop the Nyquist bin.
5. 128 HTK-mel triangles over 20 to 8000 Hz; `ln(max(e, 1.1920929e-7))`.
6. Pad or truncate to 1024 frames (padding zeros are added after the log).
7. Normalize `(x + 4.2677393) / (4.5689974 * 2)`. Model input `[1, 1024, 128]`.

**Long files:** 10.24 s windows (163840 samples) with a 5 s hop, at most 8 windows; sigmoid outputs are max-aggregated across windows.

**Tag selection** (`select_tags`):

1. `sigmoid` over the 527 logits. AST is multi-label, so classes are independent; softmax would force competition between them. Labels with probability at least `min_ast_score = 0.35` are candidates (at most 20).
2. Remap AudioSet names to editor-friendly tags (`assets/ast_label_map.json`); labels mapped to the empty string are suppressed. Tags are capped at 30 characters.
3. CLAP cross-validation: `score = 0.4 * ast_prob + 0.6 * clap_cosine`, where `clap_cosine` is the cosine between the stored audio embedding and the text embedding of the tag. Tags below `clap_validation_floor = 0.18` are dropped. A tag whose words appear in the filename is boosted by 1.3.
4. The top 6 tags by score are stored in `suggested_tags`. Suggestions are not part of the FTS index until the user confirms them. Without CLAP, tags are produced without cross-validation.

## Embedding model versioning

`embeddings.model` records which checkpoint produced a vector. The current value is `xenova-clap-htsat-unfused`. Every consumer (semantic search, similar sounds, mood, SFX counts, radar, UMAP, `has_embedding`, stats) reads only rows with the current value, so a checkpoint change is handled by writing new rows under a new identifier and re-running Radar generation; re-embedding a file overwrites its row. Re-tune thresholds (below) after any CLAP or AST checkpoint change.

## Thresholds

| Constant | Value | Where |
|---|---|---|
| `min_ast_score` | 0.35 | AST candidate cut-off |
| `ast_weight` / `clap_weight` | 0.4 / 0.6 | Composite score |
| `clap_validation_floor` | 0.18 | Drop tags CLAP contradicts |
| `filename_boost` | 1.3 | Tag words found in filename |
| `top_k` | 6 | Suggestions stored |
| `SFX_THRESHOLD` | 0.22 | SFX category membership |

The method and data behind these numbers are in [THRESHOLD_TUNING.md](THRESHOLD_TUNING.md).

## UMAP (radar)

Pure Rust, deterministic for a fixed seed, no BLAS dependency. Pipeline: drop rows that are not 512-d or are non-finite and L2-normalize the rest; exact brute-force kNN on the unit sphere (rayon); fuzzy simplicial set (smooth-kNN, probabilistic-union symmetrization); PCA initial layout by power iteration; seeded single-threaded SGD layout. Parameters: `n_neighbors` 15, `min_dist` 0.1, seed 42, epochs 500 for up to 10000 points else 200. Fewer than 16 points use the PCA layout. Output is centered and uniformly scaled so the largest absolute coordinate is 1.

The radar job embeds every sound that lacks a current-model embedding in chunks of 32 (files that fail to decode are counted in `failed`), then runs UMAP and stores the result in `radar_coords`.

## Transient detection

Pure-Rust port of `librosa.onset.onset_detect` defaults: resample to 22050 Hz mono, `n_fft` 2048, hop 512, 128 Slaney mels, power to dB, spectral flux, peak picking without backtracking. Results are returned as fractions of the file duration, at most 80 ticks. A golden test compares onsets (within one hop, counts within 10%) against librosa output on clips synthesized in code; the fixtures hold the reference onset times only.

## Indexing

`indexer.rs` walks each root with `walkdir`, extracts metadata in parallel with `rayon` and `symphonia`, and writes rows in transactions of 100 files. It is additive: rows are upserted by `path` (the row id is preserved, so tags, favorites, ratings, embeddings, suggested tags, radar coordinates and projects stay attached), nothing is deleted, and vanished files keep their rows. Cancel is cooperative and takes effect at a batch boundary; committed batches are kept. Roots with at least one processed file are registered in `indexed_roots` in a final transaction. That table, together with the sounds table, is the allow-list for `/audio/stream`, which canonicalizes the requested path before checking it. Corrupt files are added without audio details and reported in `recent_errors`. One run at a time (`409 already_running`).

## Export

The clip is decoded to f32, cut sample-exactly at `[start_ms, end_ms)` (rounded to frames at the source rate), optionally passed through one of the FX presets, and written with `hound`: 24-bit PCM for sources of 24 bits or more, 32-bit float for float sources, 16-bit otherwise; sample rate and channel count are preserved. Files go to `<home>/exports/` with collision-safe names. The desktop shell starts native drag-out with `tauri-plugin-drag`.

## Packaging

See [DESKTOP_PACKAGING.md](DESKTOP_PACKAGING.md).
