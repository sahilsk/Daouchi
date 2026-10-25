//! AudioSet tagging with the Audio Spectrogram Transformer (ONNX), label remap and optional CLAP
//! cross-validation. Port of `ai/ast_engine.py`; the scoring logic is pure (`select_tags`) so it can
//! be tested without a model.
//!
//! Model: `Xenova/ast-finetuned-audioset-10-10-0.4593` (`input_values` f32 [1,1024,128] -> `logits` [1,527]).
//! Windows are run one at a time (batch 1; required for CoreML static shapes).
use crate::fbank;
use anyhow::{anyhow, bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const NUM_CLASSES: usize = 527;
/// 10.24 s window (1024 fbank frames) at 16 kHz.
pub const WINDOW_SAMPLES: usize = 163_840;
pub const HOP_SAMPLES: usize = 80_000; // 5 s
pub const MAX_WINDOWS: usize = 8;

static LABELS_JSON: &str = include_str!("../assets/ast_labels.json");
static LABEL_MAP_JSON: &str = include_str!("../assets/ast_label_map.json");

/// Tunables (defaults come from docs/THRESHOLD_TUNING.md; re-tune when the CLAP model changes).
#[derive(Debug, Clone)]
pub struct AstConfig {
    pub min_ast_score: f32,
    pub clap_validation_floor: f32,
    pub ast_weight: f32,
    pub clap_weight: f32,
    pub filename_boost: f32,
    pub candidates: usize,
    pub top_k: usize,
    pub fallback_top_n: usize,
    pub max_tag_len: usize,
    pub max_windows: usize,
}

impl Default for AstConfig {
    fn default() -> Self {
        Self {
            min_ast_score: 0.35,
            clap_validation_floor: 0.18, // re-tuned for clap-htsat-unfused (was 0.45 on LAION); see docs/THRESHOLD_TUNING.md
            ast_weight: 0.4,
            clap_weight: 0.6,
            filename_boost: 1.3,
            candidates: 20,
            top_k: 6,
            fallback_top_n: 5,
            max_tag_len: 30,
            max_windows: MAX_WINDOWS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuggestedTag {
    pub tag: String,
    pub score: f32,
}

/// Cross-validation hook implemented by the CLAP engine (kept decoupled from the clap module).
pub trait ClapScorer {
    /// Cosine similarity between the stored audio embedding and the text embedding of `tag`.
    fn tag_similarity(&self, audio_embedding: &[f32], tag: &str) -> Result<f32>;
}

pub fn labels() -> &'static [String] {
    static L: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    L.get_or_init(|| serde_json::from_str(LABELS_JSON).expect("ast_labels.json"))
}

fn label_map() -> &'static HashMap<String, String> {
    static M: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let v: Vec<(String, String)> = serde_json::from_str(LABEL_MAP_JSON).expect("ast_label_map.json");
        v.into_iter().collect()
    })
}

/// Lowercase, strip everything but `[a-z0-9 _-]`, collapse whitespace, truncate.
pub fn normalise_tag(label: &str, max_len: usize) -> String {
    let lowered = label.to_lowercase();
    let kept: String = lowered.trim().chars().filter(|c| matches!(c, 'a'..='z' | '0'..='9' | ' ' | '_' | '-')).collect();
    let joined = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(max_len).collect()
}

/// AudioSet label -> production tag; `None` = suppressed.
pub fn remap_label(raw: &str, max_len: usize) -> Option<String> {
    let key = raw.to_lowercase();
    let key = key.trim();
    let out = match label_map().get(key) {
        Some(v) => v.clone(),
        None => normalise_tag(raw, max_len),
    };
    if out.is_empty() { None } else { Some(out) }
}

/// Start offsets of the analysis windows: one window for short clips, otherwise up to `max_windows`
/// evenly spread windows (spacing <= 5 s while the cap is not hit).
pub fn plan_windows(len: usize, max_windows: usize) -> Vec<usize> {
    if len <= WINDOW_SAMPLES {
        return vec![0];
    }
    let span = len - WINDOW_SAMPLES;
    let n = (span.div_ceil(HOP_SAMPLES) + 1).min(max_windows.max(2));
    (0..n).map(|i| i * span / (n - 1)).collect()
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

fn stem_words(name: &str) -> HashSet<String> {
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name).to_lowercase();
    stem.replace(['_', '-'], " ").split_whitespace().map(str::to_owned).collect()
}

/// Pure scoring pipeline: probabilities (527) -> top-`top_k` tags. `file_name` may be a path or stem.
/// CLAP is used only when both `audio_embedding` and `clap` are given; any scorer error makes the whole
/// clip fall back to AST-only scores (when text embedding fails).
pub fn select_tags(
    probs: &[f32],
    file_name: Option<&str>,
    audio_embedding: Option<&[f32]>,
    clap: Option<&dyn ClapScorer>,
    cfg: &AstConfig,
) -> Vec<SuggestedTag> {
    let lbls = labels();
    let n = probs.len().min(lbls.len());
    let mut idx: Vec<usize> = (0..n).filter(|&i| probs[i] >= cfg.min_ast_score).collect();
    if idx.is_empty() {
        let mut all: Vec<usize> = (0..n).collect();
        all.sort_by(|&a, &b| probs[b].total_cmp(&probs[a]));
        all.truncate(cfg.fallback_top_n);
        idx = all;
    }
    idx.sort_by(|&a, &b| probs[b].total_cmp(&probs[a]));
    idx.truncate(cfg.candidates);

    let words = file_name.map(stem_words).unwrap_or_default();
    struct Cand {
        tag: String,
        ast: f32,
        boost: f32,
    }
    let mut seen = HashSet::new();
    let mut cands = Vec::new();
    for i in idx {
        let Some(tag) = remap_label(&lbls[i], cfg.max_tag_len) else { continue };
        if !seen.insert(tag.clone()) {
            continue;
        }
        let boost = if tag.split_whitespace().any(|w| words.contains(w)) { cfg.filename_boost } else { 1.0 };
        cands.push(Cand { tag, ast: probs[i], boost });
    }
    if cands.is_empty() {
        return vec![];
    }

    let mut scored: Vec<(String, f32)> = match (audio_embedding, clap) {
        (Some(emb), Some(c)) => {
            let sims: Result<Vec<f32>> = cands.iter().map(|k| c.tag_similarity(emb, &k.tag)).collect();
            match sims {
                Ok(s) if s.iter().all(|v| v.is_finite()) => cands
                    .iter()
                    .zip(s)
                    .map(|(k, sim)| (k.tag.clone(), (cfg.ast_weight * k.ast + cfg.clap_weight * sim) * k.boost))
                    .filter(|(_, sc)| *sc >= cfg.clap_validation_floor)
                    .collect(),
                other => {
                    if let Err(e) = other {
                        tracing::warn!("CLAP cross-validation failed, using AST scores only: {e:#}");
                    }
                    cands.iter().map(|k| (k.tag.clone(), k.ast * k.boost)).collect()
                }
            }
        }
        _ => cands.iter().map(|k| (k.tag.clone(), k.ast * k.boost)).collect(),
    };
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(cfg.top_k);
    scored.into_iter().map(|(tag, s)| SuggestedTag { tag, score: (s * 10_000.0).round() / 10_000.0 }).collect()
}

pub struct AstEngine {
    session: Mutex<Session>,
    pub config: AstConfig,
}

impl AstEngine {
    /// Load `<model_dir>/onnx/model.onnx` (manifest layout) or `<model_dir>/model.onnx` (flat)
    /// with default runtime options.
    pub fn load(model_dir: &Path) -> Result<Self> {
        let nested = model_dir.join("onnx").join("model.onnx");
        let path = if nested.is_file() { nested } else { model_dir.join("model.onnx") };
        Self::load_file(&path, &crate::runtime::SessionOpts::default())
    }

    pub fn load_file(model_path: &Path, opts: &crate::runtime::SessionOpts) -> Result<Self> {
        let session = crate::runtime::session(model_path, opts)
            .with_context(|| format!("load AST model {}", model_path.display()))?;
        Ok(Self { session: Mutex::new(session), config: AstConfig::default() })
    }

    /// Sigmoid probabilities (527) for one prepared 1024x128 feature block.
    pub fn probs_for_features(&self, feats: Vec<f32>) -> Result<Vec<f32>> {
        let t = Tensor::from_array(([1usize, fbank::MAX_FRAMES, fbank::NUM_MEL], feats))?;
        let mut s = self.session.lock().map_err(|_| anyhow!("AST session poisoned"))?;
        let out = s.run(ort::inputs!["input_values" => t])?;
        let (_, d) = out["logits"].try_extract_tensor::<f32>()?;
        if d.len() != NUM_CLASSES {
            bail!("unexpected logits size {}", d.len());
        }
        Ok(d.iter().map(|&v| sigmoid(v)).collect())
    }

    /// Per-class max-aggregated sigmoid probabilities for 16 kHz mono samples in [-1, 1].
    pub fn probs(&self, samples: &[f32]) -> Result<Vec<f32>> {
        let mut agg = vec![0f32; NUM_CLASSES];
        for start in plan_windows(samples.len(), self.config.max_windows) {
            let end = (start + WINDOW_SAMPLES).min(samples.len());
            let p = self.probs_for_features(fbank::ast_features(&samples[start..end]))?;
            for (a, v) in agg.iter_mut().zip(p) {
                *a = a.max(v);
            }
        }
        Ok(agg)
    }

    /// Full auto-tag pipeline. `file_name` (path or stem) enables the filename boost; `audio_embedding`
    /// + `clap` enable cross-validation.
    pub fn tag(
        &self,
        samples: &[f32],
        file_name: Option<&str>,
        audio_embedding: Option<&[f32]>,
        clap: Option<&dyn ClapScorer>,
    ) -> Result<Vec<SuggestedTag>> {
        let p = self.probs(samples)?;
        Ok(select_tags(&p, file_name, audio_embedding, clap, &self.config))
    }
}

/// Test helper: model dir from `DAOUCHI_TEST_MODELS/ast` or the research download.
pub fn find_test_model(variant: &str) -> Option<PathBuf> {
    let mut c = vec![];
    if let Ok(d) = std::env::var("DAOUCHI_TEST_MODELS") {
        c.push(PathBuf::from(&d).join("ast").join(variant).join("model.onnx"));
        c.push(PathBuf::from(&d).join("ast").join("model.onnx"));
        c.push(PathBuf::from(&d).join("ast").join("onnx").join("model.onnx"));
    }
    c.push(PathBuf::from(format!("/tmp/daouchi-research/ast/dl/x_{variant}/model.onnx")));
    c.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(f32);
    impl ClapScorer for Fixed {
        fn tag_similarity(&self, _: &[f32], _: &str) -> Result<f32> {
            Ok(self.0)
        }
    }
    struct Failing;
    impl ClapScorer for Failing {
        fn tag_similarity(&self, _: &[f32], _: &str) -> Result<f32> {
            Err(anyhow!("no clap"))
        }
    }

    fn probs_with(pairs: &[(usize, f32)]) -> Vec<f32> {
        let mut p = vec![0.01; NUM_CLASSES];
        for &(i, v) in pairs {
            p[i] = v;
        }
        p
    }

    #[test]
    fn labels_and_map_load() {
        assert_eq!(labels().len(), NUM_CLASSES);
        let lower: HashSet<String> = labels().iter().map(|l| l.to_lowercase()).collect();
        assert!(label_map().keys().all(|k| lower.contains(k)), "dead LABEL_MAP key");
        assert_eq!(label_map().len(), 184);
    }

    #[test]
    fn remap_and_normalise() {
        assert_eq!(remap_label("Bark", 30).as_deref(), Some("dog bark"));
        assert_eq!(remap_label("Noise", 30), None);
        assert_eq!(remap_label("Fart", 30).as_deref(), Some("fart"));
        assert_eq!(remap_label("Ska", 30).as_deref(), Some("ska"));
        assert_eq!(normalise_tag("  Hello, World!! ", 30), "hello world");
        assert_eq!(normalise_tag("abcdefghij", 4), "abcd");
    }

    #[test]
    fn windows() {
        assert_eq!(plan_windows(1000, 8), vec![0]);
        assert_eq!(plan_windows(WINDOW_SAMPLES, 8), vec![0]);
        let w = plan_windows(WINDOW_SAMPLES + 100_000, 8);
        assert_eq!(w, vec![0, 50_000, 100_000]);
        let w = plan_windows(16_000 * 600, 8);
        assert_eq!(w.len(), 8);
        assert_eq!(*w.last().unwrap(), 16_000 * 600 - WINDOW_SAMPLES);
    }

    #[test]
    fn threshold_dedupe_and_fallback() {
        // Bark(75) and Bow-wow(78) -> distinct; Dog(74) -> 'dog'; 'Howl' etc. under threshold
        let p = probs_with(&[(74, 0.9), (75, 0.8), (500, 0.6)]); // Silence(500) suppressed
        let t = select_tags(&p, None, None, None, &AstConfig::default());
        assert_eq!(t.iter().map(|x| x.tag.as_str()).collect::<Vec<_>>(), ["dog", "dog bark"]);
        // nothing >= threshold -> fallback top-5
        let low = probs_with(&[(74, 0.2), (75, 0.1)]);
        let t = select_tags(&low, None, None, None, &AstConfig::default());
        assert_eq!(t.len(), 5);
        assert_eq!(t[0].tag, "dog");
    }

    #[test]
    fn filename_boost_and_rounding() {
        let p = probs_with(&[(74, 0.5), (81, 0.6)]); // dog / cat
        let t = select_tags(&p, Some("/x/My_Dog-sound.wav"), None, None, &AstConfig::default());
        assert_eq!(t[0].tag, "dog"); // 0.5*1.3 = 0.65 > 0.6
        assert_eq!(t[0].score, 0.65);
    }

    #[test]
    fn clap_composite_floor_and_failure() {
        let p = probs_with(&[(74, 0.8), (81, 0.7)]);
        let cfg = AstConfig::default();
        let emb = [1.0f32; 4];
        let t = select_tags(&p, None, Some(&emb), Some(&Fixed(0.5)), &cfg);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].score, ((0.4f32 * 0.8 + 0.6 * 0.5) * 10_000.0).round() / 10_000.0);
        // strongly negative similarity -> everything below the floor is dropped
        assert!(select_tags(&p, None, Some(&emb), Some(&Fixed(-0.5)), &cfg).is_empty());
        // scorer error -> AST-only
        assert_eq!(select_tags(&p, None, Some(&emb), Some(&Failing), &cfg).len(), 2);
        // no embedding -> no floor
        assert_eq!(select_tags(&p, None, None, Some(&Fixed(-0.5)), &cfg).len(), 2);
    }
}
