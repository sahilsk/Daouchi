# Daouchi - Multi-Agent Build Strategy

How work on Daouchi is split across parallel agent tracks, who owns which files, and how tracks stay compatible. The product and its internals are described in `README.md` and `docs/ARCHITECTURE.md`; the API in `contracts.md`; the database in `schema.sql`.

---

## Principles

1. **Contract first.** `contracts.md` (HTTP API) and `schema.sql` (database) are the shared interface. A track implements against them and never invents field names, routes or error shapes. Changing a contract is its own, reviewed change that lands before the code depending on it.
2. **One owner per file.** Each file has exactly one owning track. A track never edits another track's files; it requests the change through the contract or the owner.
3. **Disjoint tracks run in parallel.** Core, AI engines, frontend, shell and docs share only the contract surface, so they can proceed concurrently.
4. **Everything verifiable.** Every track has a command that proves its work (see Verification).

---

## Tracks

### Track A - Core server and API (Rust, `src/`)

Router and handlers, SQLite access, indexer, request transport, engine orchestration.

Owns: `src/lib.rs`, `src/transport.rs`, `src/engines.rs`, `src/indexer.rs`, `src/lifecycle.rs`, `src/db.rs`, `src/db_migrate.rs`, `src/integ_tests.rs`, `src/api/*.rs`.

Rules: the DB mutex guard is dropped before every `.await`; inference runs under `spawn_blocking`; a missing model is `409 model_not_ready`, a failed engine start is `503 engine_unavailable`; indexing stays additive; no socket is ever bound.

### Track B - AI engines (`crates/daouchi-ai/`)

ONNX Runtime bootstrap and sessions, model manager (manifests, resumable sha256-verified downloads), CLAP, AST, Kaldi fbank, UMAP, transient detection, audio decode, storage layout (`DaouchiHome`).

Owns: everything under `crates/daouchi-ai/` (sources, `assets/`, `tests/`, `examples/`). Within the crate, one module per owner: `home`, `models`, `runtime` (foundation); `audio`, `clap` (CLAP); `fbank`, `ast` (AST); `umap`; `transients`.

Rules: no HTTP and no database in this crate; preprocessing is pinned by golden tests against reference outputs; thresholds change only with the evidence described in `docs/THRESHOLD_TUNING.md`.

### Track C - Frontend (`web/`)

Alpine.js UI: library, search and filters, waveform player, dashboard, radar, projects, model download gate.

Owns: `web/index.html`, `web/bootstrap.html`, `web/style.css`, `web/app.js`, `web/components/*.js`, `web/vendor/` (pinned libraries, versions and hashes in `web/vendor/VERSIONS.md`).

Rules: call only routes documented in `contracts.md`; no network requests to third-party hosts (the UI works offline); desktop-only behavior is gated on `window.__TAURI__`.

### Track D - Desktop shell and infrastructure

Tauri window, URI scheme handler, drag-out, capabilities, packaging and run scripts.

Owns: `crates/daouchi-desktop/`, `Cargo.toml`, `Cargo.lock`, `start.sh`, `dev.sh`, `scripts/`.

### Track E - Documentation

Owns: `README.md`, `AGENTS.md`, `contracts.md`, `docs/`. `contracts.md` is edited by Track E on behalf of the track that changes an endpoint, and always together with that change. `schema.sql` is changed only together with a migration in `src/db_migrate.rs`.

---

## File ownership map

```
Cargo.toml, Cargo.lock                  D
src/lib.rs, transport.rs, engines.rs    A
src/indexer.rs, lifecycle.rs            A
src/db.rs, db_migrate.rs                A
src/integ_tests.rs                      A
src/api/*.rs                            A
schema.sql                              A (with a migration)
crates/daouchi-ai/**                    B
crates/daouchi-desktop/**               D
web/** (incl. vendor/)                  C
start.sh, dev.sh, scripts/**            D
README.md, AGENTS.md, contracts.md      E
docs/**                                 E
```

---

## Runtime dependency graph

```
web/ (Alpine.js)  --daouchi:// scheme-->  crates/daouchi-desktop
                                              |  in-process
                                              v
                                       src/ axum Router --> SQLite
                                              |
                                              v
                                       crates/daouchi-ai (ONNX Runtime)
```

Dependencies point downward only: the AI crate does not know the core; the core does not know the shell; the frontend knows only the HTTP contract.

---

## Adding a track

1. Specify any new endpoints, tables or IPC shapes in `contracts.md` and `schema.sql` first.
2. Choose a file-ownership boundary that overlaps no existing track, and add it to the ownership map above.
3. Brief the new agent with: the contract documents, the ownership map, the verification commands for its area, and which existing endpoints it may call but not modify.
4. Run it in parallel with every track whose files it does not touch.

When a change genuinely spans tracks (for example a new endpoint plus UI), do it in order: contract, then the implementing track, then the consuming track, then a single integration pass that runs the full verification set.

---

## Verification

```bash
cargo check --workspace
cargo test --workspace --lib                         # unit tests, no models required
cargo test -p daouchi-ai                             # adds golden tests on synthesized clips; model-backed ones skip without models
DAOUCHI_TEST_MODELS=/path/to/models cargo test -p daouchi-ai          # golden tests against real models (DAOUCHI_TEST_COREML=1 adds the CoreML checks)
DAOUCHI_INTEG=1 DAOUCHI_HOME=/tmp/daouchi-integ cargo test -p daouchi integ -- --nocapture   # router integration (DAOUCHI_INTEG_MODELS=1 with preseeded models)
bash -n start.sh dev.sh scripts/bundle-macos.sh      # script syntax
```

For a UI change, run `./dev.sh` (UI served from `web/` on disk) and exercise the affected view.

---

## Working agreements

- **No ephemeral artifacts in the repository.** Models, caches, exports, build output, databases and scratch files live in `~/.daouchi`, `$CARGO_TARGET_DIR` or a temp directory. `.gitignore` covers them; do not add exceptions.
- **exFAT drive hygiene.** The project drive can be exFAT, which creates `._*` sidecar files. Sweep them before finishing:
  `find . \( -path ./.git -o -path ./.claude/worktrees \) -prune -o -name '._*' -type f -delete`
- **Build output on the internal disk.** Set `CARGO_TARGET_DIR` (the scripts default it to `~/.cache/daouchi-target`).
- **Golden tests against reference outputs.** Preprocessing (CLAP mel, AST fbank, onset detection) and the engines are verified against numeric outputs of the reference implementation (PyTorch/transformers/librosa) stored in `crates/daouchi-ai/tests/fixtures/`. A numerical change needs a regenerated reference, not a loosened tolerance. No audio files are committed: every test clip is synthesized by `tests/common/mod.rs` (integer LCG, sines, sweeps, decaying partials, filtered noise, click trains), and fixtures hold only reference outputs (feature samples, probabilities, embeddings, token ids, onset times).
  To regenerate: re-implement the generator in `tests/common/mod.rs` in the reference environment (the `generator_parity` test pins fingerprints that the reference generator must reproduce), run the reference model/feature extractor/onset detector on each named clip, and write the outputs as little-endian f32 or JSON under `tests/fixtures/{ast,clap,transients}/` (every 8th AST frame, every 10th CLAP mel frame, embeddings rounded to 6 decimals; keep the total well under 1 MB). Then update the fingerprints in `tests/generator_parity.rs` if the generator changed. Python is needed only for this offline step, never at runtime.
- **Additive migrations.** Schema changes are additive, versioned with `PRAGMA user_version`, and never drop user tables.
- **Docs move with code.** A route, environment variable, path or command changes in the same change as its documentation. Documentation describes the current product only; forward-looking items belong in the README roadmap.
- **No git operations by track agents.** Staging, committing and pushing are done by the coordinating session.
