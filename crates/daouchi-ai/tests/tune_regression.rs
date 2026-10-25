//! Pins the re-tuned AST/CLAP cross-validation defaults (see docs/THRESHOLD_TUNING.md) on a few cached
//! cached cases (AST probabilities + clap-htsat-unfused cosines captured by examples/tune_thresholds).
use anyhow::{anyhow, Result};
use daouchi_ai::ast::{select_tags, AstConfig, ClapScorer, NUM_CLASSES};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize)]
struct Case {
    name: String,
    top: Vec<(usize, f32)>,
    cos: HashMap<String, f32>,
}
struct Table(HashMap<String, f32>);
impl ClapScorer for Table {
    fn tag_similarity(&self, _: &[f32], tag: &str) -> Result<f32> {
        self.0.get(tag).copied().ok_or_else(|| anyhow!("no cos for {tag}"))
    }
}

fn run(c: &Case, cfg: &AstConfig) -> Vec<String> {
    let mut p = vec![0.001f32; NUM_CLASSES];
    for &(i, v) in &c.top {
        p[i] = v;
    }
    select_tags(&p, Some(&c.name), Some(&[1.0; 4]), Some(&Table(c.cos.clone())), cfg).into_iter().map(|t| t.tag).collect()
}

#[test]
fn retuned_defaults_keep_confident_tags() {
    let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/tune_cases.json")).unwrap();
    let old = AstConfig { clap_validation_floor: 0.45, ..Default::default() };
    let got: Vec<Vec<String>> = cases.iter().map(|c| run(c, &AstConfig::default())).collect();
    let was: usize = cases.iter().map(|c| run(c, &old).len()).sum();
    println!("{got:?} old_total={was}");
    // every case has a confident AST label and must keep >= 1 tag
    assert!(got.iter().all(|g| !g.is_empty()), "{got:?}");
    assert!(got.iter().map(Vec::len).sum::<usize>() > was);
    let want: [&[&str]; 4] = [&["speech"], &["music", "theme music", "scary music", "ambient"], &["drip", "plop"], &["music", "whoosh", "sonar"]];
    for (g, w) in got.iter().zip(want) {
        assert_eq!(g.iter().map(String::as_str).collect::<Vec<_>>(), w);
    }
}
