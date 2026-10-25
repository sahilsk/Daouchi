//! AST end to end vs PyTorch fp32 sigmoid probabilities on synthesized clips.
//! Needs the AST ONNX (`DAOUCHI_TEST_MODELS/ast[/onnx]/model.onnx`, see `ast::find_test_model`) and ONNX
//! Runtime (`ORT_DYLIB_PATH`); skips with a message otherwise. `DAOUCHI_TEST_COREML=1` adds the CoreML path.
mod common;
use common::*;
use daouchi_ai::ast::{find_test_model, AstEngine, NUM_CLASSES};
use daouchi_ai::runtime::{self, SessionOpts};

fn top(p: &[f32], k: usize) -> Vec<usize> {
    let mut i: Vec<usize> = (0..p.len()).collect();
    i.sort_by(|&a, &b| p[b].partial_cmp(&p[a]).unwrap().then(a.cmp(&b)));
    i.truncate(k);
    i
}

/// Top-k sets are equal except for classes within `tie` of the k-th reference probability.
fn assert_top_equal(name: &str, got: &[f32], want: &[f32], k: usize, tie: f32) {
    let (gt, wt) = (top(got, k), top(want, k));
    let kth = want[wt[k - 1]];
    for &i in &gt {
        assert!(want[i] >= kth - tie, "{name}: rust top-{k} has class {i} (ref {}) below ref k-th {kth}", want[i]);
    }
    for &i in &wt {
        assert!(got[i] >= got[gt[k - 1]] - tie, "{name}: ref top-{k} class {i} missing from rust top-{k}");
    }
    if gt != wt {
        eprintln!("{name}: top-{k} order differs only through near-ties: rust {gt:?} ref {wt:?}");
    }
}

fn run(coreml: bool, tol: f32) {
    let Some(model) = find_test_model("fp32") else {
        eprintln!("SKIP ast_golden: AST ONNX not found (set DAOUCHI_TEST_MODELS=<dir with ast/model.onnx>)");
        return;
    };
    if runtime::find_dylib().is_err() {
        eprintln!("SKIP ast_golden: ONNX Runtime library not found (set ORT_DYLIB_PATH)");
        return;
    }
    warnings::install();
    runtime::init().unwrap();
    let engine = AstEngine::load_file(&model, &SessionOpts { coreml, ..Default::default() }).unwrap();
    if coreml {
        warnings::assert_no_coreml_fallback();
    }
    for name in AST_CLIPS {
        let want = read_f32(&fixture_dir("ast").join(format!("{name}.probs.f32")));
        assert_eq!(want.len(), NUM_CLASSES);
        let got = engine.probs(&ast_clip(name)).unwrap();
        let maxd = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        eprintln!("ast coreml={coreml} {name:6}: max |prob diff| = {maxd:.2e}");
        assert!(maxd < tol, "{name}: max prob diff {maxd}");
        assert_top_equal(name, &got, &want, 6, tol * 2.0);
    }
}

#[test]
fn ast_cpu_matches_pytorch() {
    run(false, 1e-3);
}

#[test]
fn ast_coreml_matches_pytorch() {
    if std::env::var_os("DAOUCHI_TEST_COREML").is_none() {
        eprintln!("SKIP ast_coreml_matches_pytorch: set DAOUCHI_TEST_COREML=1 to enable");
        return;
    }
    if std::env::var_os("DAOUCHI_HOME").is_none() {
        std::env::set_var("DAOUCHI_HOME", std::env::temp_dir().join("daouchi-test-home"));
    }
    run(true, 1e-2);
}
