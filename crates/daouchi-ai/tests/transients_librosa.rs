//! Onset detection vs `librosa.onset.onset_detect` (units="time") on synthesized 22.05 kHz clips.
//! Pass criteria: >= 90% of reference onsets have a Rust onset within +-1 hop (512 samples), and the
//! onset count is within +-10% (at least +-1) on every clip.
mod common;
use common::*;
use daouchi_ai::transients::detect_samples;
use std::collections::BTreeMap;

const HOP_SECS: f64 = 512.0 / 22050.0;

#[test]
fn onsets_match_librosa() {
    let gold: BTreeMap<String, Vec<f64>> =
        serde_json::from_slice(&std::fs::read(fixture_dir("transients").join("golden.json")).unwrap()).unwrap();
    assert_eq!(gold.len(), TRANSIENT_CLIPS.len());
    let (mut total, mut matched) = (0usize, 0usize);
    for name in TRANSIENT_CLIPS {
        let x = transient_clip(name);
        let dur = x.len() as f64 / TR_SR as f64;
        let got: Vec<f64> = detect_samples(&x, TR_SR).iter().map(|f| *f as f64 * dur).collect();
        let want = &gold[name];
        let hits = want.iter().filter(|t| got.iter().any(|g| (g - **t).abs() <= HOP_SECS + 1e-9)).count();
        let tol = ((want.len() as f64 * 0.1).ceil() as usize).max(1);
        eprintln!("{name:14} librosa {:2} rust {:2} matched {hits}", want.len(), got.len());
        assert!(
            got.len().abs_diff(want.len()) <= tol,
            "{name}: count {} vs librosa {} (tol {tol})\n rust   {got:?}\n librosa {want:?}",
            got.len(),
            want.len()
        );
        total += want.len();
        matched += hits;
    }
    let frac = matched as f64 / total as f64;
    eprintln!("matched {matched}/{total} = {:.1}%", frac * 100.0);
    assert!(frac >= 0.9, "only {matched}/{total} onsets within +-1 hop");
}
