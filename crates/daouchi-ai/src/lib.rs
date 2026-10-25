//! daouchi-ai: in-process AI engines Each module has one owner; do not edit others' modules.
pub mod home;      // owner: foundation  — ~/.daouchi layout, DAOUCHI_HOME
pub mod models;    // owner: foundation  — manifest, on-demand download, sha256 verify
pub mod runtime;   // owner: foundation  — ONNX Runtime init (load-dynamic), sessions, CoreML EP
pub mod audio;     // owner: clap        — decode, resample, mono, int16 round trip, windowing
pub mod clap;      // owner: clap        — HTSAT audio + RoBERTa text embeddings, semantic ranking
pub mod fbank;     // owner: ast         — Kaldi-style fbank
pub mod ast;       // owner: ast         — AudioSet tagging, label remap, CLAP cross-validation
pub mod umap;      // owner: umap        — kNN + PCA init + umap-rs, 512d -> 2d
pub mod transients; // owner: transients — librosa-compatible onset detection
