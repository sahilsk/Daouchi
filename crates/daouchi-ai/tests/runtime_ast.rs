//! Loads the AST ONNX through `runtime::session` with and without CoreML, prints I/O, runs a dummy
//! inference and records latency. Skips if the model (or the ORT dylib) is absent.
//! Model: $DAOUCHI_TEST_AST_MODEL or /tmp/daouchi-research/ast/dl/x_fp32/model.onnx
use daouchi_ai::runtime::{self, SessionOpts};
use std::path::PathBuf;
use std::time::Instant;

fn infer(s: &mut ort::session::Session, x: &[f32]) -> Vec<f32> {
    let name = s.inputs[0].name.clone();
    let t = ort::value::Tensor::from_array(([1usize, 1024, 128], x.to_vec())).unwrap();
    let out = s.run(ort::inputs![name.as_str() => t]).unwrap();
    out[0].try_extract_tensor::<f32>().unwrap().1.to_vec()
}

#[test]
fn ast_cpu_vs_coreml() {
    let model = PathBuf::from(std::env::var("DAOUCHI_TEST_AST_MODEL").unwrap_or("/tmp/daouchi-research/ast/dl/x_fp32/model.onnx".into()));
    if !model.is_file() || runtime::find_dylib().is_err() {
        eprintln!("skip: model or ORT dylib missing");
        return;
    }
    // persistent CoreML compile cache for the test (first compile is slow)
    if std::env::var_os("DAOUCHI_HOME").is_none() {
        std::env::set_var("DAOUCHI_HOME", "/tmp/daouchi-work/foundation/home");
    }
    runtime::init().unwrap();
    eprintln!("ORT dylib: {:?}", runtime::dylib_in_use());
    let x: Vec<f32> = (0..1024 * 128).map(|i| (((i * 2654435761usize) >> 8) % 1000) as f32 / 1000.0 - 0.5).collect();

    let t = Instant::now();
    let mut cpu = runtime::session(&model, &SessionOpts { coreml: false, ..Default::default() }).unwrap();
    eprintln!("CPU load {:?}", t.elapsed());
    for i in &cpu.inputs { eprintln!("input  {} {:?}", i.name, i.input_type); }
    for o in &cpu.outputs { eprintln!("output {} {:?}", o.name, o.output_type); }
    let _ = infer(&mut cpu, &x);
    let t = Instant::now();
    let n = 5;
    let mut ref_out = vec![];
    for _ in 0..n { ref_out = infer(&mut cpu, &x); }
    let cpu_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    assert!(ref_out.iter().all(|v| v.is_finite()));
    eprintln!("CPU: {} logits, {:.1} ms/inference", ref_out.len(), cpu_ms);

    if cfg!(target_os = "macos") {
        let t = Instant::now();
        let mut ml = runtime::session(&model, &SessionOpts::default()).unwrap();
        eprintln!("CoreML load (incl. compile or cache hit) {:?}", t.elapsed());
        let t = Instant::now();
        let first = infer(&mut ml, &x);
        eprintln!("CoreML first inference {:?}", t.elapsed());
        let t = Instant::now();
        let mut out = first;
        for _ in 0..n { out = infer(&mut ml, &x); }
        let ml_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let maxdiff = ref_out.iter().zip(&out).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        eprintln!("CoreML: {:.1} ms/inference, max |logit diff| vs CPU = {:.2e}  (speedup x{:.1})", ml_ms, maxdiff, cpu_ms / ml_ms);
        assert!(maxdiff < 0.1, "CoreML output diverges from CPU");
    }
}
