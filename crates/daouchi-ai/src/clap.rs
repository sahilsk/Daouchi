//! CLAP engine: Xenova/clap-htsat-unfused ONNX (audio + text) with Rust front-ends.
use crate::audio;
use anyhow::{anyhow, Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

mod mel;
pub use mel::{clip_features, log_mel, CLIP_FRAMES, CLIP_SAMPLES, N_MELS, SAMPLE_RATE};

pub const EMBED_DIM: usize = 512;
pub const TEXT_LEN: usize = 77;
pub const TEXT_PREFIX: &str = "this is a sound of ";
/// SFX category similarity threshold. Re-tuned for clap-htsat-unfused (cosines are lower than LAION's,
/// where 0.25 was used); see docs/THRESHOLD_TUNING.md.
pub const SFX_THRESHOLD: f32 = 0.22;
pub const SFX_CATEGORY_QUERIES: [(&str, &str); 5] = [
    ("riser", "this is a sound of riser sweep ascending pitch transition buildup"),
    ("impact", "this is a sound of impact hit boom explosion cinematic sub thud"),
    ("atmosphere", "this is a sound of atmospheric drone ambient texture room tone environmental background"),
    ("foley", "this is a sound of foley footstep organic everyday sound real world"),
    ("whoosh", "this is a sound of whoosh swoosh fast motion air movement transition"),
];

/// How to handle audio longer than 10 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LongAudio {
    /// Deterministic 10 s center crop (default).
    #[default]
    CenterCrop,
    /// Mean of embeddings over non-overlapping 10 s chunks, then L2.
    ChunkMean,
}

pub struct ClapEngine {
    audio: Mutex<Session>,
    text: Mutex<Session>,
    tokenizer: Tokenizer,
    pub long_audio: LongAudio,
}

impl ClapEngine {
    /// Load from a model directory. Accepts the manifest layout (`onnx/{audio,text}_model.onnx`
    /// plus `tokenizer.json` at the root) or the flat layout (all three side by side).
    pub fn load(model_dir: &Path) -> Result<Self> {
        Self::load_with(model_dir, &crate::runtime::SessionOpts::default())
    }

    pub fn load_with(model_dir: &Path, opts: &crate::runtime::SessionOpts) -> Result<Self> {
        crate::runtime::init()?;
        let audio_path = resolve_model_file(model_dir, "audio_model.onnx");
        let text_path = resolve_model_file(model_dir, "text_model.onnx");
        // The audio tower always sees one `[1, 1, CLIP_FRAMES, N_MELS]` log-mel clip; pinning those symbolic
        // dims lets CoreML take the whole graph (pinning height/width to 1 makes shape inference fail).
        let mut audio_opts = opts.clone();
        for (name, size) in [
            ("audio_batch_size", 1),
            ("num_channels", 1),
            ("height", CLIP_FRAMES as i64),
            ("width", N_MELS as i64),
        ] {
            audio_opts.dim_overrides.push((name.to_string(), size));
        }
        let audio = crate::runtime::session(&audio_path, &audio_opts)?;
        let text = crate::runtime::session(&text_path, &crate::runtime::SessionOpts {
            static_shapes: false,
            ..opts.clone()
        })?;
        let tokenizer = build_tokenizer(&model_dir.join("tokenizer.json"))?;
        Ok(Self { audio: Mutex::new(audio), text: Mutex::new(text), tokenizer, long_audio: LongAudio::default() })
    }

    /// Embed an audio file (decoded, mono, 48 kHz). Returns an L2-normalized 512-vector.
    pub fn embed_audio_file(&self, path: &Path) -> Result<[f32; EMBED_DIM]> {
        let mono = audio::load_mono(path, SAMPLE_RATE)?;
        self.embed_audio_samples(&mono)
    }

    /// Embed 48 kHz mono samples in [-1,1].
    pub fn embed_audio_samples(&self, mono48k: &[f32]) -> Result<[f32; EMBED_DIM]> {
        if mono48k.is_empty() {
            return Err(anyhow!("empty audio"));
        }
        if self.long_audio == LongAudio::ChunkMean && mono48k.len() > CLIP_SAMPLES {
            let mut acc = [0.0f32; EMBED_DIM];
            let wins = audio::chunk_windows(mono48k, CLIP_SAMPLES);
            for w in &wins {
                let e = self.run_audio(&clip_features(w))?;
                for (a, v) in acc.iter_mut().zip(e.iter()) {
                    *a += v;
                }
            }
            return l2_normalized(&acc);
        }
        l2_normalized(&self.run_audio(&clip_features(mono48k))?)
    }

    /// Run the audio tower on a precomputed `[1001*64]` log-mel; returns the raw (unnormalized) embedding.
    pub fn run_audio(&self, feats: &[f32]) -> Result<[f32; EMBED_DIM]> {
        let t = Tensor::from_array(([1usize, 1, CLIP_FRAMES, N_MELS], feats.to_vec()))?;
        let mut s = self.audio.lock().map_err(|_| anyhow!("audio session poisoned"))?;
        let out = s.run(ort::inputs!["input_features" => t])?;
        let (_, d) = out["audio_embeds"].try_extract_tensor::<f32>()?;
        first_row(d)
    }

    /// Text embedding (prefixed with "this is a sound of " unless present), L2-normalized.
    pub fn embed_text(&self, query: &str) -> Result<[f32; EMBED_DIM]> {
        let ids = self.token_ids(query)?;
        let t = Tensor::from_array(([1usize, TEXT_LEN], ids))?;
        let mut s = self.text.lock().map_err(|_| anyhow!("text session poisoned"))?;
        let out = s.run(ort::inputs!["input_ids" => t])?;
        let (_, d) = out["text_embeds"].try_extract_tensor::<f32>()?;
        l2_normalized(&first_row(d)?)
    }

    pub fn token_ids(&self, query: &str) -> Result<Vec<i64>> {
        let enc = self.tokenizer.encode(prefixed_query(query), true).map_err(|e| anyhow!("tokenize: {e}"))?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
        if ids.len() != TEXT_LEN {
            return Err(anyhow!("tokenizer produced {} ids, expected {TEXT_LEN}", ids.len()));
        }
        Ok(ids)
    }

    /// Per-category count of rows whose cosine with the category query >= SFX_THRESHOLD.
    pub fn sfx_category_counts<'a>(
        &self,
        rows: impl Iterator<Item = (i64, &'a [f32])> + Clone,
    ) -> Vec<(&'static str, usize)> {
        let qs: Vec<(&'static str, Option<[f32; EMBED_DIM]>)> =
            SFX_CATEGORY_QUERIES.iter().map(|(c, q)| (*c, self.embed_text(q).ok())).collect();
        sfx_counts_with(&qs, rows)
    }

    pub fn model_dir_hint() -> PathBuf {
        PathBuf::from("clap")
    }
}

fn first_row(d: &[f32]) -> Result<[f32; EMBED_DIM]> {
    if d.len() < EMBED_DIM {
        return Err(anyhow!("model output has {} values", d.len()));
    }
    let mut a = [0.0f32; EMBED_DIM];
    a.copy_from_slice(&d[..EMBED_DIM]);
    Ok(a)
}

/// Build the "this is a sound of ..." string exactly like clap_engine.py `_embed_text`.
pub fn prefixed_query(query: &str) -> String {
    let stripped = query.trim();
    let rest = if stripped.len() >= TEXT_PREFIX.len()
        && stripped.is_char_boundary(TEXT_PREFIX.len())
        && stripped[..TEXT_PREFIX.len()].eq_ignore_ascii_case(TEXT_PREFIX)
    {
        &stripped[TEXT_PREFIX.len()..]
    } else {
        stripped
    };
    format!("{TEXT_PREFIX}{rest}")
}

fn build_tokenizer(path: &Path) -> Result<Tokenizer> {
    let mut tok = Tokenizer::from_file(path).map_err(|e| anyhow!("tokenizer {}: {e}", path.display()))?;
    let pad_id = tok.token_to_id("<pad>").or_else(|| tok.get_padding().map(|p| p.pad_id)).unwrap_or(1);
    let pad_token = tok.id_to_token(pad_id).unwrap_or_else(|| "<pad>".into());
    tok.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::Fixed(TEXT_LEN),
        pad_id,
        pad_token,
        ..Default::default()
    }));
    tok.with_truncation(Some(TruncationParams { max_length: TEXT_LEN, ..Default::default() }))
        .map_err(|e| anyhow!("truncation: {e}"))?;
    Ok(tok)
}

pub fn l2_normalized(v: &[f32; EMBED_DIM]) -> Result<[f32; EMBED_DIM]> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !n.is_finite() || n == 0.0 {
        return Err(anyhow!("degenerate embedding (norm {n})"));
    }
    let mut o = *v;
    o.iter_mut().for_each(|x| *x /= n);
    Ok(o)
}

// ---- DB BLOB helpers (f32 little-endian x512) ----

pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Returns None unless the blob is exactly 512 f32 values.
pub fn bytes_to_vec(b: &[u8]) -> Option<Vec<f32>> {
    if b.len() != EMBED_DIM * 4 {
        return None;
    }
    Some(b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

// ---- ranking ----

/// Cosine-rank rows against `query` (matches clap_engine.py semantic_search): rows that are not
/// 512-long or contain non-finite values are dropped; scores use the norm+1e-8 cosine form.
/// Returns up to `k` (id, score) sorted descending.
pub fn rank<'a>(query: &[f32], rows: impl Iterator<Item = (i64, &'a [f32])>, k: usize) -> Vec<(i64, f32)> {
    if query.len() != EMBED_DIM || !query.iter().all(|x| x.is_finite()) || k == 0 {
        return Vec::new();
    }
    let qn = norm(query) + 1e-8;
    let mut scored: Vec<(i64, f32)> = rows
        .filter(|(_, v)| v.len() == EMBED_DIM && v.iter().all(|x| x.is_finite()))
        .map(|(id, v)| {
            let dot: f32 = v.iter().zip(query).map(|(a, b)| a * b).sum();
            (id, (dot / (norm(v) + 1e-8)) / qn)
        })
        .collect();
    let k = k.min(scored.len());
    if k == 0 {
        return Vec::new();
    }
    let cmp = |a: &(i64, f32), b: &(i64, f32)| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0));
    if k < scored.len() {
        scored.select_nth_unstable_by(k - 1, cmp);
        scored.truncate(k);
    }
    scored.sort_by(cmp);
    scored
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Count rows per category with cosine >= [`SFX_THRESHOLD`] given precomputed category query vectors.
pub fn sfx_counts_with<'a>(
    queries: &[(&'static str, Option<[f32; EMBED_DIM]>)],
    rows: impl Iterator<Item = (i64, &'a [f32])> + Clone,
) -> Vec<(&'static str, usize)> {
    queries
        .iter()
        .map(|(cat, q)| {
            let n = match q {
                None => 0,
                Some(q) => {
                    let qn = norm(q) + 1e-8;
                    rows.clone()
                        .filter(|(_, v)| v.len() == EMBED_DIM && v.iter().all(|x| x.is_finite()))
                        .filter(|(_, v)| {
                            let dot: f32 = v.iter().zip(q.iter()).map(|(a, b)| a * b).sum();
                            (dot / (norm(v) + 1e-8)) / qn >= SFX_THRESHOLD
                        })
                        .count()
                }
            };
            (*cat, n)
        })
        .collect()
}

/// Prefer `<dir>/onnx/<name>` (manifest layout), falling back to `<dir>/<name>` (flat layout).
fn resolve_model_file(model_dir: &Path, name: &str) -> std::path::PathBuf {
    let nested = model_dir.join("onnx").join(name);
    if nested.is_file() {
        nested
    } else {
        model_dir.join(name)
    }
}

/// Convenience: check the model files exist (either layout).
pub fn ensure_files(model_dir: &Path) -> Result<()> {
    for f in ["audio_model.onnx", "text_model.onnx", "tokenizer.json"] {
        let p = if f == "tokenizer.json" { model_dir.join(f) } else { resolve_model_file(model_dir, f) };
        p.metadata().with_context(|| format!("missing {}", p.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_both_layouts() {
        let d = std::env::temp_dir().join(format!("daouchi-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("onnx")).unwrap();
        assert_eq!(resolve_model_file(&d, "audio_model.onnx"), d.join("audio_model.onnx"));
        std::fs::write(d.join("audio_model.onnx"), b"x").unwrap();
        assert_eq!(resolve_model_file(&d, "audio_model.onnx"), d.join("audio_model.onnx"));
        std::fs::write(d.join("onnx").join("audio_model.onnx"), b"x").unwrap();
        assert_eq!(resolve_model_file(&d, "audio_model.onnx"), d.join("onnx").join("audio_model.onnx"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn prefix_logic() {
        assert_eq!(prefixed_query("dog bark"), "this is a sound of dog bark");
        assert_eq!(prefixed_query("  This Is A Sound Of rain "), "this is a sound of rain");
        assert_eq!(prefixed_query("this is a sound of"), "this is a sound of this is a sound of");
    }

    #[test]
    fn blob_round_trip() {
        let v: Vec<f32> = (0..512).map(|i| i as f32 * 0.5).collect();
        assert_eq!(bytes_to_vec(&vec_to_bytes(&v)).unwrap(), v);
        assert!(bytes_to_vec(&[0u8; 100]).is_none());
    }

    #[test]
    fn rank_filters_and_orders() {
        let mut q = vec![0.0f32; 512];
        q[0] = 1.0;
        let mut a = vec![0.0f32; 512];
        a[0] = 2.0;
        let mut b = vec![0.0f32; 512];
        b[0] = 1.0;
        b[1] = 1.0;
        let mut bad = a.clone();
        bad[3] = f32::NAN;
        let short = vec![1.0f32; 10];
        let rows: Vec<(i64, &[f32])> = vec![(1, &b), (2, &a), (3, &bad), (4, &short)];
        let r = rank(&q, rows.into_iter(), 10);
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![2, 1]);
        assert!((r[0].1 - 1.0).abs() < 1e-5);
        assert!((r[1].1 - 0.7071).abs() < 1e-3);
    }

    #[test]
    fn mel_shape_and_silence() {
        let f = clip_features(&vec![0.0; 1000]);
        assert_eq!(f.len(), CLIP_FRAMES * N_MELS);
        assert!(f.iter().all(|&v| (v + 100.0).abs() < 1e-3));
    }
}
