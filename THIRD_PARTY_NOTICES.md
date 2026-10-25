# Third-party notices

Daouchi itself is MIT licensed (see `LICENSE`). It builds on the software and models below, each under its own terms.

## Bundled in the repository

| Component | Version | License | Upstream |
|---|---|---|---|
| Alpine.js (`web/vendor/alpine-*.min.js`) | 3.17.5 | MIT | https://github.com/alpinejs/alpine |
| WaveSurfer.js (`web/vendor/wavesurfer-*.min.js`) | 7.12.12 | BSD-3-Clause | https://github.com/katspaugh/wavesurfer.js |
| Tailwind CSS play build (`web/vendor/tailwind-play-*.js`) | 3.4.17 | MIT | https://github.com/tailwindlabs/tailwindcss |

Exact files and checksums are listed in `web/vendor/VERSIONS.md`.

## Linked at runtime (not in the repository)

| Component | License | Notes |
|---|---|---|
| ONNX Runtime shared library | MIT | Installed separately or bundled by `scripts/bundle-macos.sh` into the app's `Frameworks` folder. https://github.com/microsoft/onnxruntime |

## Models downloaded on demand

Model files are fetched into `~/.daouchi/models` the first time a feature needs them. They are not part of this repository and keep their original licenses.

| Model | Used for | License | Source |
|---|---|---|---|
| CLAP HTSAT-tiny (ONNX export) | Semantic, mood and SFX-category search | Apache-2.0 | https://huggingface.co/laion/clap-htsat-unfused, ONNX export https://huggingface.co/Xenova/clap-htsat-unfused |
| Audio Spectrogram Transformer, AudioSet fine-tuned (ONNX export) | Auto-tag suggestions | BSD-3-Clause | https://huggingface.co/MIT/ast-finetuned-audioset-10-10-0.4593, ONNX export https://huggingface.co/Xenova/ast-finetuned-audioset-10-10-0.4593 |

## Rust dependencies

Rust crates are pulled in through Cargo (see `Cargo.lock`); the major ones are Tauri, axum, tokio, rusqlite (bundled SQLite), symphonia, rustfft/realfft, rubato, hound, ort and tokenizers, all under permissive licenses (MIT, Apache-2.0 or BSD). To list every dependency license, run `cargo install cargo-about` and `cargo about generate` against the workspace.
