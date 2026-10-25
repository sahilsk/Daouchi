//! Threshold tuning harness for the AST/CLAP cross-validation and SFX category thresholds.
//!
//!   tune_thresholds cache <files.tsv> <out.json> <clap_model_dir> <ast_model.onnx>
//!       files.tsv rows: id <TAB> path <TAB> collection <TAB> full(1|0)   (full=0 -> CLAP embedding only)
//!   tune_thresholds sweep <cache.json> <old_tags.tsv>
//!       old_tags.tsv rows: id <TAB> tag   (reference tags; may be empty)
//!
//! All model outputs are cached so thresholds can be swept offline. See docs/THRESHOLD_TUNING.md.
use anyhow::Result;
use daouchi_ai::ast::{self, AstConfig, AstEngine, ClapScorer};
use daouchi_ai::clap::{ClapEngine, SFX_CATEGORY_QUERIES};
use daouchi_ai::{audio, runtime};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Serialize, Deserialize)]
struct Item {
    id: i64,
    path: String,
    collection: String,
    emb: Vec<f32>,
    probs: Option<Vec<f32>>,
}
#[derive(Serialize, Deserialize)]
struct Cache {
    items: Vec<Item>,
    text: BTreeMap<String, Vec<f32>>,
    cats: BTreeMap<String, Vec<f32>>,
}

struct CachedScorer<'a>(&'a BTreeMap<String, Vec<f32>>);
impl ClapScorer for CachedScorer<'_> {
    fn tag_similarity(&self, a: &[f32], tag: &str) -> Result<f32> {
        let t = self.0.get(tag).ok_or_else(|| anyhow::anyhow!("no text emb for {tag}"))?;
        Ok(cos(a, t))
    }
}
fn cos(a: &[f32], b: &[f32]) -> f32 {
    let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    d / (na * nb + 1e-8)
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("cache") => cache(&a[2], &a[3], &a[4], &a[5]),
        Some("sweep") => sweep(&a[2], &a[3]),
        _ => anyhow::bail!("usage: cache|sweep ..."),
    }
}

fn cache(tsv: &str, out: &str, clap_dir: &str, ast_path: &str) -> Result<()> {
    runtime::init()?;
    let opts = runtime::SessionOpts { intra_threads: 2, coreml: false, static_shapes: false, ..Default::default() };
    let clap = ClapEngine::load_with(Path::new(clap_dir), &opts)?;
    let ast = AstEngine::load_file(Path::new(ast_path), &opts)?;
    let rows: Vec<(i64, String, String, bool)> = std::fs::read_to_string(tsv)?
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 4).then(|| (f[0].parse().unwrap_or(0), f[1].into(), f[2].into(), f[3] == "1"))
        })
        .collect();
    let total = rows.len();
    let done = std::sync::atomic::AtomicUsize::new(0);
    let items: Vec<Item> = rows
        .par_iter()
        .filter_map(|(id, path, coll, full)| {
            let r = (|| -> Result<Item> {
                let emb = clap.embed_audio_file(Path::new(path))?.to_vec();
                let probs = if *full {
                    let s = audio::load_mono(Path::new(path), 16_000)?;
                    Some(ast.probs(&s)?)
                } else {
                    None
                };
                Ok(Item { id: *id, path: path.clone(), collection: coll.clone(), emb, probs })
            })();
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n % 25 == 0 {
                eprintln!("{n}/{total}");
            }
            match r {
                Ok(i) => Some(i),
                Err(e) => {
                    eprintln!("skip {path}: {e:#}");
                    None
                }
            }
        })
        .collect();
    let mut text = BTreeMap::new();
    for l in ast::labels() {
        if let Some(t) = ast::remap_label(l, 30) {
            if !text.contains_key(&t) {
                text.insert(t.clone(), clap.embed_text(&t)?.to_vec());
            }
        }
    }
    let mut cats = BTreeMap::new();
    for (c, q) in SFX_CATEGORY_QUERIES {
        cats.insert(c.to_string(), clap.embed_text(q)?.to_vec());
    }
    std::fs::write(out, serde_json::to_vec(&Cache { items, text, cats })?)?;
    Ok(())
}

fn pct(v: &mut Vec<f32>, p: f32) -> f32 {
    if v.is_empty() {
        return f32::NAN;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f32 * p).round() as usize]
}

fn sweep(cache: &str, old: &str) -> Result<()> {
    let c: Cache = serde_json::from_slice(&std::fs::read(cache)?)?;
    let mut oldt: HashMap<i64, HashSet<String>> = HashMap::new();
    for l in std::fs::read_to_string(old)?.lines() {
        if let Some((i, t)) = l.split_once('\t') {
            oldt.entry(i.parse().unwrap_or(0)).or_default().insert(t.to_string());
        }
    }
    let full: Vec<&Item> = c.items.iter().filter(|i| i.probs.is_some()).collect();
    println!("items={} full={} ref-files={}", c.items.len(), full.len(), full.iter().filter(|i| oldt.contains_key(&i.id)).count());
    let sc = CachedScorer(&c.text);

    // --- cosine distribution: audio vs AST-confident tags, vs all other tags of the same file
    let (mut pos, mut neg) = (vec![], vec![]);
    for it in &full {
        let p = it.probs.as_ref().unwrap();
        for (i, l) in ast::labels().iter().enumerate() {
            let Some(t) = ast::remap_label(l, 30) else { continue };
            let s = cos(&it.emb, &c.text[&t]);
            if p[i] >= 0.5 { pos.push(s) } else if p[i] < 0.05 { neg.push(s) }
        }
    }
    let auc = {
        let mut pairs = 0u64;
        let mut win = 0f64;
        let mut ns = neg.clone();
        ns.sort_by(|a, b| a.total_cmp(b));
        for &p in &pos {
            let k = ns.partition_point(|&n| n < p);
            win += k as f64;
            pairs += ns.len() as u64;
        }
        win / pairs as f64
    };
    let (mut p1, mut n1) = (pos.clone(), neg.clone());
    println!("cos(audio,tag) AST-confident(>=.5) n={} p10/50/90={:.3}/{:.3}/{:.3}  other(<.05) n={} p10/50/90={:.3}/{:.3}/{:.3} AUC={:.3}",
        pos.len(), pct(&mut p1, 0.1), pct(&mut p1, 0.5), pct(&mut p1, 0.9), neg.len(), pct(&mut n1, 0.1), pct(&mut n1, 0.5), pct(&mut n1, 0.9), auc);

    // --- sweep over weights/floors
    let mut rows = vec![];
    for &(aw, cw) in &[(0.4f32, 0.6f32), (0.5, 0.5), (0.6, 0.4), (0.7, 0.3), (0.8, 0.2)] {
        for fl in (10..=60).step_by(2).map(|x| x as f32 / 100.0) {
            let cfg = AstConfig { ast_weight: aw, clap_weight: cw, clap_validation_floor: fl, ..Default::default() };
            rows.push((aw, cw, fl, eval(&full, &c, &sc, &cfg, &oldt)));
        }
    }
    let base_cfg = AstConfig::default();
    let base = eval(&full, &c, &sc, &base_cfg, &oldt);
    println!("BASELINE (old defaults) {}", base.fmt());
    let none = AstConfig { clap_validation_floor: -1.0, ..Default::default() };
    println!("NO FLOOR                {}", eval(&full, &c, &sc, &none, &oldt).fmt());
    for r in &rows {
        if (r.2 * 100.0).round() as i32 % 4 == 2 || r.0 == 0.4 {
            println!("w={:.1}/{:.1} floor={:.2} {}", r.0, r.1, r.2, r.3.fmt());
        }
    }

    // --- SFX
    println!("\nSFX category counts over {} embeddings (cosine thresholds)", c.items.len());
    let cos_dist: Vec<(String, Vec<f32>)> = c
        .cats
        .iter()
        .map(|(k, q)| (k.clone(), c.items.iter().map(|i| cos(&i.emb, q)).collect()))
        .collect();
    for (k, v) in &cos_dist {
        let mut s = v.clone();
        println!("{k:11} cos p5/25/50/75/95/max = {:.3}/{:.3}/{:.3}/{:.3}/{:.3}/{:.3}",
            pct(&mut s, 0.05), pct(&mut s, 0.25), pct(&mut s, 0.5), pct(&mut s, 0.75), pct(&mut s, 0.95), pct(&mut s, 1.0));
    }
    for th in [0.10f32, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50] {
        let counts: Vec<String> = cos_dist.iter().map(|(k, v)| format!("{k}={}", v.iter().filter(|&&x| x >= th).count())).collect();
        println!("th={th:.2}  {}", counts.join(" "));
    }
    // relative rule: top-p quantile of each category, plus centered (cos - mean over library) rule
    let n = c.items.len();
    let means: Vec<f32> = cos_dist.iter().map(|(_, v)| v.iter().sum::<f32>() / n as f32).collect();
    let sds: Vec<f32> = cos_dist.iter().zip(&means).map(|((_, v), m)| (v.iter().map(|x| (x - m).powi(2)).sum::<f32>() / n as f32).sqrt()).collect();
    println!("means={:?} sds={:?}", means, sds);
    for z in [0.5f32, 1.0, 1.5, 2.0] {
        let counts: Vec<String> = cos_dist.iter().enumerate().map(|(j, (k, v))| format!("{k}={}", v.iter().filter(|&&x| x >= means[j] + z * sds[j]).count())).collect();
        println!("z>={z:.1}  {}", counts.join(" "));
    }
    Ok(())
}

struct Metrics {
    coverage: f32,
    avg_tags: f32,
    ref_p: f32,
    ref_r: f32,
    ref_f1: f32,
    strong_keep: f32,
    junk: f32, // fraction of kept tags whose AST prob < 0.2 (tag came only via filename/clap, i.e. weak AST support)
    conf_cov: f32, // coverage among files with AST max >= 0.5
}
impl Metrics {
    fn fmt(&self) -> String {
        format!("cov={:.3} conf_cov={:.3} tags/file={:.2} junk={:.3} strong_keep={:.3} ref P/R/F1={:.3}/{:.3}/{:.3}", self.coverage, self.conf_cov, self.avg_tags, self.junk, self.strong_keep, self.ref_p, self.ref_r, self.ref_f1)
    }
}
fn eval(full: &[&Item], c: &Cache, sc: &CachedScorer, cfg: &AstConfig, oldt: &HashMap<i64, HashSet<String>>) -> Metrics {
    let (mut cov, mut tags, mut conf, mut conf_cov) = (0, 0, 0, 0);
    let (mut kept, mut junk, mut strong, mut strong_kept) = (0usize, 0usize, 0usize, 0usize);
    let (mut tp, mut fp, mut fnn) = (0f32, 0f32, 0f32);
    let _ = c;
    for it in full {
        let p = it.probs.as_ref().unwrap();
        let t = ast::select_tags(p, Some(&it.path), Some(&it.emb), Some(sc), cfg);
        if !t.is_empty() { cov += 1 }
        tags += t.len();
        if p.iter().cloned().fold(0., f32::max) >= 0.5 {
            conf += 1;
            if !t.is_empty() { conf_cov += 1 }
        }
        // junk = kept tag with weak AST support (<0.5) AND no CLAP support (cos < 0.05)
        let mut best: HashMap<String, f32> = HashMap::new();
        for (i, l) in ast::labels().iter().enumerate() {
            if let Some(tg) = ast::remap_label(l, 30) {
                let e = best.entry(tg).or_insert(0.0);
                *e = e.max(p[i]);
            }
        }
        for x in &t {
            kept += 1;
            if best[&x.tag] < 0.5 && cos(&it.emb, &c.text[&x.tag]) < 0.05 { junk += 1 }
        }
        for (tg, &a) in &best {
            if a >= 0.5 {
                strong += 1;
                if t.iter().any(|x| &x.tag == tg) { strong_kept += 1 }
            }
        }
        if let Some(r) = oldt.get(&it.id) {
            let got: HashSet<&str> = t.iter().map(|x| x.tag.as_str()).collect();
            let m = r.iter().filter(|x| got.contains(x.as_str())).count() as f32;
            tp += m;
            fp += got.len() as f32 - m;
            fnn += r.len() as f32 - m;
        }
    }
    let n = full.len() as f32;
    let (pr, rc) = (tp / (tp + fp).max(1.0), tp / (tp + fnn).max(1.0));
    Metrics { coverage: cov as f32 / n, avg_tags: tags as f32 / n, ref_p: pr, ref_r: rc, ref_f1: 2.0 * pr * rc / (pr + rc).max(1e-9), junk: junk as f32 / kept.max(1) as f32, strong_keep: strong_kept as f32 / strong.max(1) as f32, conf_cov: conf_cov as f32 / conf.max(1) as f32 }
}
