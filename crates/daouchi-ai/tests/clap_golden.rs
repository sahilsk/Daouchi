//! CLAP front-end and engine vs the reference (HF `ClapFeatureExtractor` + Xenova ONNX via onnxruntime).
//! Clips are synthesized in code (see `common`); fixtures hold numeric outputs only.
//! Model tests need `DAOUCHI_TEST_MODELS=<dir with audio_model.onnx, text_model.onnx, tokenizer.json>` (or its
//! `clap/` subdirectory) and ONNX Runtime (`ORT_DYLIB_PATH`); they skip with a message otherwise.
mod common;
use common::*;
use daouchi_ai::audio::resample_mono;
use daouchi_ai::clap::{
    bytes_to_vec, clip_features, rank, vec_to_bytes, ClapEngine, LongAudio, CLIP_FRAMES, EMBED_DIM, N_MELS, SAMPLE_RATE,
};
use daouchi_ai::runtime::{self, SessionOpts};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Deserialize)]
struct ClipGold {
    audio_embeds: Vec<f32>,
    chunk_mean_embeds: Option<Vec<f32>>,
}
#[derive(Deserialize)]
struct TextGold {
    query: String,
    ids: Vec<i64>,
    embeds: Vec<f32>,
    rank: Vec<(String, f32)>,
}
#[derive(Deserialize)]
struct Gold {
    clips: BTreeMap<String, ClipGold>,
    text: Vec<TextGold>,
}

fn gold() -> Gold {
    serde_json::from_slice(&std::fs::read(fixture_dir("clap").join("golden.json")).unwrap()).unwrap()
}

/// Mono 48 kHz input exactly as the engine would receive it from a decoded file.
fn mono48(name: &str) -> Vec<f32> {
    let c = clap_clip(name);
    resample_mono(&c.mono_f32(), c.sr, SAMPLE_RATE).unwrap()
}

fn model_dir() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var("DAOUCHI_TEST_MODELS").ok()?);
    [d.clone(), d.join("clap")].into_iter().find(|p| daouchi_ai::clap::ensure_files(p).is_ok())
}

fn engine(coreml: bool) -> Option<ClapEngine> {
    let Some(dir) = model_dir() else {
        eprintln!("SKIP: CLAP models not found (set DAOUCHI_TEST_MODELS=<dir with audio_model.onnx, text_model.onnx, tokenizer.json>)");
        return None;
    };
    if runtime::find_dylib().is_err() {
        eprintln!("SKIP: ONNX Runtime library not found (set ORT_DYLIB_PATH)");
        return None;
    }
    Some(ClapEngine::load_with(&dir, &SessionOpts { coreml, ..Default::default() }).unwrap())
}

fn unit(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

#[test]
fn log_mel_matches_hf_input_features() {
    let mut checked = 0;
    for name in CLAP_CLIPS {
        let c = clap_clip(name);
        if c.sr != 48000 {
            continue; // resampled clips are covered by the embedding test (resampler differences dominate)
        }
        let feats = clip_features(&c.mono_f32());
        assert_eq!(feats.len(), CLIP_FRAMES * N_MELS);
        let want = read_f32(&fixture_dir("clap").join(format!("{name}.mel10.f32")));
        assert_eq!(want.len(), 101 * N_MELS, "{name}: fixture size");
        let mut maxd = 0f32;
        for (r, row) in want.chunks_exact(N_MELS).enumerate() {
            let g = &feats[r * 10 * N_MELS..(r * 10 + 1) * N_MELS];
            maxd = g.iter().zip(row).map(|(a, b)| (a - b).abs()).fold(maxd, f32::max);
        }
        eprintln!("mel {name:16}: max |diff| = {maxd:.2e} dB");
        assert!(maxd < 2e-3, "{name}: max diff {maxd}");
        checked += 1;
    }
    assert!(checked >= 4);
}

#[test]
fn audio_embeddings_match_reference() {
    let Some(e) = engine(false) else { return };
    let g = gold();
    for name in CLAP_CLIPS {
        let got = e.embed_audio_samples(&mono48(name)).unwrap();
        let want = unit(&g.clips[name].audio_embeds);
        let cos = cosine(&got, &want);
        // resampler-limited clips: rubato (Rust) vs torchaudio (reference) differ slightly before the model
        let floor = if name == "chirp_44k" || name == "noise_22k" { 0.99 } else { 0.999 };
        eprintln!("audio {name:16}: cosine = {cos:.6} (floor {floor})");
        assert!(cos > floor, "{name}: cosine {cos}");
    }
}

#[test]
fn long_audio_modes() {
    let Some(mut e) = engine(false) else { return };
    let g = gold();
    let x = mono48("long_48k");
    assert!(x.len() > 3 * 480_000 && x.len() < 4 * 480_000);
    let center = e.embed_audio_samples(&x).unwrap(); // default: center crop
    let c = cosine(&center, &unit(&g.clips["long_48k"].audio_embeds));
    eprintln!("long center-crop cosine = {c:.6}");
    assert!(c > 0.999);
    e.long_audio = LongAudio::ChunkMean;
    let mean = e.embed_audio_samples(&x).unwrap();
    let c2 = cosine(&mean, &unit(g.clips["long_48k"].chunk_mean_embeds.as_ref().unwrap()));
    eprintln!("long chunk-mean cosine = {c2:.6}");
    assert!(c2 > 0.999);
    assert!(cosine(&center, &mean) < 0.9999, "modes should differ");
    // chunk mean over a short clip equals the single-clip embedding
    let short = mono48("sweep_48k");
    let a = e.embed_audio_samples(&short).unwrap();
    e.long_audio = LongAudio::CenterCrop;
    assert!(cosine(&a, &e.embed_audio_samples(&short).unwrap()) > 0.99999);
}

#[test]
fn text_embeddings_and_tokens_match_reference() {
    let Some(e) = engine(false) else { return };
    for t in gold().text {
        assert_eq!(e.token_ids(&t.query).unwrap(), t.ids, "token ids for {:?}", t.query);
        let got = e.embed_text(&t.query).unwrap();
        let cos = cosine(&got, &t.embeds);
        eprintln!("text {:40.40}: cosine = {cos:.6}", t.query);
        assert!(cos > 0.999, "{:?}: cosine {cos}", t.query);
    }
}

#[test]
fn blob_and_rank_helpers_on_reference_vectors() {
    let g = gold();
    let rows: Vec<(String, Vec<f32>)> = g.clips.iter().map(|(n, c)| (n.clone(), unit(&c.audio_embeds))).collect();
    for (_, v) in &rows {
        assert_eq!(v.len(), EMBED_DIM);
        assert_eq!(bytes_to_vec(&vec_to_bytes(v)).unwrap(), *v);
    }
    // ranking the reference text vectors over the reference audio vectors reproduces the reference order
    for t in &g.text {
        let it = rows.iter().enumerate().map(|(i, (_, v))| (i as i64, v.as_slice()));
        let ranked = rank(&t.embeds, it, rows.len());
        assert_eq!(ranked.len(), rows.len());
        for (pos, (id, score)) in ranked.iter().enumerate() {
            let (gname, gscore) = &t.rank[pos];
            assert!((score - gscore).abs() < 1e-4, "{:?}: score {score} vs {gscore}", t.query);
            let near_tie = (pos > 0 && (t.rank[pos - 1].1 - gscore).abs() < 1e-3)
                || (pos + 1 < t.rank.len() && (t.rank[pos + 1].1 - gscore).abs() < 1e-3);
            assert!(near_tie || rows[*id as usize].0 == *gname, "{:?}: rank {pos}", t.query);
        }
        // top-k truncation
        assert_eq!(rank(&t.embeds, rows.iter().enumerate().map(|(i, (_, v))| (i as i64, v.as_slice())), 2).len(), 2);
    }
}

/// Opt-in (`DAOUCHI_TEST_COREML=1`): CLAP audio tower on CoreML (static shapes via dim overrides) must
/// load without falling back to CPU and agree with the reference.
#[test]
fn coreml_audio_matches_reference() {
    if std::env::var_os("DAOUCHI_TEST_COREML").is_none() {
        eprintln!("SKIP coreml_audio_matches_reference: set DAOUCHI_TEST_COREML=1 to enable");
        return;
    }
    if std::env::var_os("DAOUCHI_HOME").is_none() {
        std::env::set_var("DAOUCHI_HOME", std::env::temp_dir().join("daouchi-test-home"));
    }
    let Some(dir) = model_dir() else { return eprintln!("SKIP: CLAP models not found") };
    warnings::install();
    runtime::init().unwrap();
    let t = std::time::Instant::now();
    let e = ClapEngine::load_with(&dir, &SessionOpts { coreml: true, ..Default::default() }).unwrap();
    warnings::assert_no_coreml_fallback();
    eprintln!("CoreML CLAP load (incl. compile or cache hit): {:?}", t.elapsed());
    let g = gold();
    let x = mono48("sweep_48k");
    let t = std::time::Instant::now();
    let first = e.embed_audio_samples(&x).unwrap();
    eprintln!("CoreML first inference: {:?}", t.elapsed());
    let n = 5;
    let t = std::time::Instant::now();
    for _ in 0..n {
        e.embed_audio_samples(&x).unwrap();
    }
    eprintln!("CoreML steady latency (mel + inference): {:.1} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    let cos = cosine(&first, &unit(&g.clips["sweep_48k"].audio_embeds));
    eprintln!("CoreML vs reference cosine = {cos:.6}");
    assert!(cos > 0.999);
}
