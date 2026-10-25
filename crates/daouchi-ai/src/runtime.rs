//! ONNX Runtime bootstrap (ort `load-dynamic`) and session factory with CoreML EP + CPU fallback.
//!
//! Requires an ONNX Runtime 1.22.x shared library (ort 2.0.0-rc.10 targets ORT API 22).
//! Discovery order: `ORT_DYLIB_PATH`, `<exe dir>/../Frameworks`, `<exe dir>`, `DaouchiHome/lib`.
use crate::home::DaouchiHome;
use anyhow::{anyhow, Context, Result};
use ort::execution_providers::coreml::{CoreMLComputeUnits, CoreMLExecutionProvider, CoreMLModelFormat};
use ort::session::{builder::GraphOptimizationLevel, Session};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
const DYLIB_NAMES: &[&str] = &["libonnxruntime.dylib"];
#[cfg(target_os = "windows")]
const DYLIB_NAMES: &[&str] = &["onnxruntime.dll"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const DYLIB_NAMES: &[&str] = &["libonnxruntime.so"];

#[derive(Debug, Clone)]
pub struct SessionOpts {
    /// Intra-op threads for the CPU EP (0 = ORT default).
    pub intra_threads: usize,
    /// Try the CoreML EP (macOS only); falls back to CPU on any failure.
    pub coreml: bool,
    /// CoreML: pin all free input dims (e.g. `batch_size`) to 1 and set static_input_shapes. Callers must then feed batch=1.
    pub static_shapes: bool,
    /// With `static_shapes`: explicit sizes for named free dims (name, size). Free dims not listed here are
    /// pinned to 1, which suits batch-only models such as AST; models with other symbolic dims must list them.
    pub dim_overrides: Vec<(String, i64)>,
}

impl Default for SessionOpts {
    fn default() -> Self {
        Self { intra_threads: 0, coreml: cfg!(target_os = "macos"), static_shapes: true, dim_overrides: Vec::new() }
    }
}

/// Locate the ORT dylib using the documented search order.
pub fn find_dylib() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("ORT_DYLIB_PATH").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        return if p.is_file() { Ok(p) } else { Err(anyhow!("ORT_DYLIB_PATH={} does not exist", p.display())) };
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.join("../Frameworks"));
            dirs.push(d.to_path_buf());
        }
    }
    if let Ok(h) = DaouchiHome::resolve() {
        dirs.push(h.lib_dir());
    }
    for d in &dirs {
        for n in DYLIB_NAMES {
            let p = d.join(n);
            if p.is_file() {
                return Ok(p.canonicalize().unwrap_or(p));
            }
        }
    }
    Err(anyhow!(
        "ONNX Runtime 1.22.x shared library not found (looked for {:?} in {:?}); set ORT_DYLIB_PATH",
        DYLIB_NAMES,
        dirs
    ))
}

extern "C" {
    fn atexit(cb: extern "C" fn()) -> i32;
}

extern "C" fn release_env_at_exit() {
    // ORT's own static destructors (registered at dlopen) run at exit; if the global OrtEnv is still alive
    // they abort with "mutex lock failed". atexit handlers run in reverse registration order, i.e. before
    // the dylib's, so release the environment here (ort never drops its static).
    use ort::AsPointer;
    if let Ok(env) = ort::environment::get_environment() {
        let ptr = env.ptr() as *mut ort::sys::OrtEnv;
        unsafe { (ort::api().ReleaseEnv)(ptr) };
    }
}

fn register_exit_release() {
    unsafe { atexit(release_env_at_exit) };
}

static INIT: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();

/// Idempotent: locate + load the ORT dylib and commit the environment.
pub fn init() -> Result<()> {
    let r = INIT.get_or_init(|| {
        (|| -> Result<PathBuf> {
            let p = find_dylib()?;
            ort::init_from(p.to_string_lossy().to_string())
                .with_name("daouchi").with_telemetry(false)
                .commit()
                .map_err(|e| anyhow!("ort init failed: {e}"))?;
            register_exit_release();
            Ok(p)
        })()
        .map_err(|e| format!("{e:#}"))
    });
    match r {
        Ok(p) => {
            tracing::debug!("ONNX Runtime dylib: {}", p.display());
            Ok(())
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}

/// Path of the dylib in use (after a successful `init`).
pub fn dylib_in_use() -> Option<PathBuf> {
    INIT.get().and_then(|r| r.as_ref().ok().cloned())
}

fn base_builder(opts: &SessionOpts) -> Result<ort::session::builder::SessionBuilder> {
    let mut b = Session::builder()
        .map_err(|e| anyhow!("{e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow!("{e}"))?;
    if opts.intra_threads > 0 {
        b = b.with_intra_threads(opts.intra_threads).map_err(|e| anyhow!("{e}"))?;
    }
    Ok(b)
}

fn load_cpu(model_path: &Path, opts: &SessionOpts) -> Result<Session> {
    base_builder(opts)?
        .commit_from_file(model_path)
        .map_err(|e| anyhow!("load {} (CPU): {e}", model_path.display()))
}

/// Names of symbolic (free) input dimensions, found by loading the graph without optimization.
fn free_dim_names(model_path: &Path) -> Result<Vec<String>> {
    let probe = Session::builder()
        .map_err(|e| anyhow!("{e}"))?
        .with_optimization_level(GraphOptimizationLevel::Disable)
        .map_err(|e| anyhow!("{e}"))?
        .commit_from_file(model_path)
        .map_err(|e| anyhow!("probe {}: {e}", model_path.display()))?;
    let mut names: Vec<String> = Vec::new();
    for i in &probe.inputs {
        if let ort::value::ValueType::Tensor { dimension_symbols, .. } = &i.input_type {
            for n in dimension_symbols.iter().filter(|n| !n.is_empty()) {
                if !names.contains(n) { names.push(n.clone()); }
            }
        }
    }
    Ok(names)
}

fn load_coreml(model_path: &Path, opts: &SessionOpts) -> Result<Session> {
    let cache = DaouchiHome::resolve()?.coreml_cache_dir();
    std::fs::create_dir_all(&cache).ok();
    let mut ep = CoreMLExecutionProvider::default()
        .with_model_format(CoreMLModelFormat::MLProgram)
        .with_compute_units(CoreMLComputeUnits::All)
        .with_model_cache_dir(cache.to_string_lossy());
    if opts.static_shapes {
        ep = ep.with_static_input_shapes(true);
    }
    let mut b = base_builder(opts)?;
    if opts.static_shapes {
        // Fix every free dimension: without this CoreML only takes a fraction of the graph. Dims named in
        // `dim_overrides` get that size; any other free dim (normally just the batch) is pinned to 1.
        for n in free_dim_names(model_path)? {
            let size = opts.dim_overrides.iter().find(|(k, _)| *k == n).map(|(_, v)| *v).unwrap_or(1);
            b = b.with_dimension_override(&n, size).map_err(|e| anyhow!("{e}"))?;
        }
    }
    b.with_execution_providers([ep.build().error_on_failure()])
        .map_err(|e| anyhow!("register CoreML EP: {e}"))?
        .commit_from_file(model_path)
        .map_err(|e| anyhow!("load {} (CoreML): {e}", model_path.display()))
}

/// Create a session. With `opts.coreml` on macOS, tries CoreML first and falls back to CPU (logged).
pub fn session(model_path: &Path, opts: &SessionOpts) -> Result<Session> {
    init()?;
    if !model_path.is_file() {
        return Err(anyhow!("model file not found: {}", model_path.display()));
    }
    if opts.coreml && cfg!(target_os = "macos") {
        match load_coreml(model_path, opts) {
            Ok(s) => return Ok(s),
            Err(e) => tracing::warn!("CoreML unavailable for {}, falling back to CPU: {e:#}", model_path.display()),
        }
    }
    load_cpu(model_path, opts).with_context(|| "CPU session")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_opts() {
        let o = SessionOpts::default();
        assert_eq!(o.intra_threads, 0);
        assert!(o.static_shapes);
    }

    #[test]
    fn missing_model_is_error() {
        if init().is_err() { return; }
        assert!(session(Path::new("/nonexistent/x.onnx"), &SessionOpts::default()).is_err());
    }

    #[test]
    fn init_idempotent_when_dylib_present() {
        if find_dylib().is_err() { eprintln!("skip: no dylib"); return; }
        init().unwrap();
        init().unwrap();
        assert!(dylib_in_use().is_some());
    }
}
