//! Export clip: trim a library sound (or one of its stems) to its in/out region
//! and write a lossless WAV at the source sample rate / channel count.
//!

use crate::api::AppState;
use axum::{extract::{Path, State}, http::StatusCode, routing::post, Json, Router};
use serde::Deserialize;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

type ApiErr = (StatusCode, String);

fn bad(msg: impl Into<String>) -> ApiErr { (StatusCode::BAD_REQUEST, msg.into()) }
fn internal(msg: impl ToString) -> ApiErr { (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()) }

/* ── Request ─────────────────────────────────────────────────────── */

#[derive(Deserialize)]
struct ExportBody {
    start_ms: Option<f64>,
    end_ms: Option<f64>,
    fx_preset: Option<String>,
    format: Option<String>,
    /// Optional stem name (vocals|drums|bass|other): export that stem WAV instead of the source.
    stem: Option<String>,
}

#[derive(Deserialize)]
struct RevealBody { path: String }

/* ── FX allowlist ────────────────────────────────────────────────── */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fx { Underwater, Telephone, ClubDrop, Lofi }

impl Fx {
    /// Parse against the allowlist. Anything else (including path separators) is rejected.
    pub fn parse(s: &str) -> Option<Fx> {
        match s {
            "underwater" => Some(Fx::Underwater),
            "telephone" => Some(Fx::Telephone),
            "clubdrop" => Some(Fx::ClubDrop),
            "lofi" => Some(Fx::Lofi),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Fx::Underwater => "underwater",
            Fx::Telephone => "telephone",
            Fx::ClubDrop => "clubdrop",
            Fx::Lofi => "lofi",
        }
    }
    fn apply(self, s: &mut [f32], sr: u32, ch: usize) {
        match self {
            Fx::Underwater => biquad_lowpass(s, sr, 450.0, 1.2, ch),
            Fx::Telephone => biquad_bandpass(s, sr, 1500.0, 3.5, ch),
            Fx::ClubDrop => biquad_lowpass(s, sr, 300.0, 0.8, ch),
            Fx::Lofi => { biquad_lowpass(s, sr, 8000.0, 0.5, ch); soft_clip(s, 3.0); }
        }
    }
}

/* ── Biquad DSP helpers ──────────────────────────────────────────── */

fn biquad_lowpass(samples: &mut [f32], sr: u32, cutoff: f32, q: f32, channels: usize) {
    let w0 = 2.0 * std::f32::consts::PI * cutoff / sr as f32;
    let alpha = w0.sin() / (2.0 * q);
    let cos_w0 = w0.cos();
    let a0 = 1.0 + alpha;
    let (b0, b1, b2) = ((1.0 - cos_w0) / 2.0 / a0, (1.0 - cos_w0) / a0, (1.0 - cos_w0) / 2.0 / a0);
    let (a1, a2) = (-2.0 * cos_w0 / a0, (1.0 - alpha) / a0);
    apply_biquad(samples, b0, b1, b2, a1, a2, channels);
}

fn biquad_bandpass(samples: &mut [f32], sr: u32, center: f32, q: f32, channels: usize) {
    let w0 = 2.0 * std::f32::consts::PI * center / sr as f32;
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    let (b0, b1, b2) = (alpha / a0, 0.0, -alpha / a0);
    let (a1, a2) = (-2.0 * w0.cos() / a0, (1.0 - alpha) / a0);
    apply_biquad(samples, b0, b1, b2, a1, a2, channels);
}

fn apply_biquad(samples: &mut [f32], b0: f32, b1: f32, b2: f32, a1: f32, a2: f32, channels: usize) {
    let ch_count = channels.max(1);
    for ch in 0..ch_count {
        let (mut x1, mut x2, mut y1, mut y2) = (0f32, 0f32, 0f32, 0f32);
        let mut i = ch;
        while i < samples.len() {
            let x0 = samples[i];
            let y0 = b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
            samples[i] = y0.clamp(-1.0, 1.0);
            x2 = x1; x1 = x0; y2 = y1; y1 = y0;
            i += ch_count;
        }
    }
}

fn soft_clip(samples: &mut [f32], drive: f32) {
    for s in samples.iter_mut() {
        let x = *s * drive;
        *s = (x - x * x * x / 3.0).clamp(-1.0, 1.0) / drive.max(1.0);
    }
}

/* ── Decode ──────────────────────────────────────────────────────── */

#[derive(Debug, Clone)]
pub struct Decoded {
    /// Interleaved samples in [-1, 1].
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: usize,
    /// Source bit depth (best effort; 16 when unknown).
    pub bits: u16,
    pub is_float: bool,
}

pub fn decode_to_f32(path: &str) -> anyhow::Result<Decoded> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    use symphonia::core::sample::SampleFormat as SymFmt;

    let src = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = FsPath::new(path).extension().and_then(|e| e.to_str()) { hint.with_extension(ext); }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or_else(|| anyhow::anyhow!("no audio track"))?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let mut sample_rate = params.sample_rate.unwrap_or(44100);
    let mut channels = params.channels.map(|c| c.count()).unwrap_or(0);

    let is_float = matches!(params.sample_format, Some(SymFmt::F32) | Some(SymFmt::F64));
    let bits: u16 = if is_float {
        32
    } else {
        params.bits_per_sample.or(params.bits_per_coded_sample)
            .map(|b| b.clamp(1, 32) as u16)
            .or(match params.sample_format {
                Some(SymFmt::S24) | Some(SymFmt::U24) => Some(24),
                Some(SymFmt::S32) | Some(SymFmt::U32) => Some(32),
                _ => None,
            })
            .unwrap_or(16)
    };

    let mut decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default())?;
    let mut out: Vec<f32> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id { continue; }
        let decoded = match decoder.decode(&packet) { Ok(d) => d, Err(_) => continue };
        let spec = *decoded.spec();
        sample_rate = spec.rate;
        channels = spec.channels.count();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        out.extend_from_slice(buf.samples());
    }
    if channels == 0 { channels = 1; }
    Ok(Decoded { samples: out, sample_rate, channels, bits, is_float })
}

/* ── Trimming ────────────────────────────────────────────────────── */

fn ms_to_frame(ms: f64, sr: u32) -> usize {
    (ms * sr as f64 / 1000.0).round().max(0.0) as usize
}

/// Compute the sample-exact frame range [start, end) for the requested ms bounds.
/// Missing start = 0, missing end = end of file; end is clamped to the file length.
pub fn frame_range(total_frames: usize, sr: u32, start_ms: Option<f64>, end_ms: Option<f64>) -> Result<(usize, usize), String> {
    for v in [start_ms, end_ms].into_iter().flatten() {
        if !v.is_finite() || v < 0.0 { return Err("start_ms/end_ms must be finite and >= 0".into()); }
    }
    let start = start_ms.map(|m| ms_to_frame(m, sr)).unwrap_or(0);
    let end = end_ms.map(|m| ms_to_frame(m, sr)).unwrap_or(total_frames).min(total_frames);
    if start >= total_frames { return Err("start_ms is past the end of the sound".into()); }
    if start >= end { return Err("start_ms must be less than end_ms".into()); }
    Ok((start, end))
}

/* ── WAV writing ─────────────────────────────────────────────────── */

pub fn write_wav(path: &FsPath, samples: &[f32], sr: u32, channels: usize, bits: u16, is_float: bool) -> anyhow::Result<()> {
    use hound::{SampleFormat, WavSpec, WavWriter};
    let (bits_per_sample, fmt) = if is_float { (32, SampleFormat::Float) }
        else if bits >= 24 { (24, SampleFormat::Int) } else { (16, SampleFormat::Int) };
    let spec = WavSpec { channels: channels as u16, sample_rate: sr, bits_per_sample, sample_format: fmt };
    let mut w = WavWriter::create(path, spec)?;
    match (fmt, bits_per_sample) {
        (SampleFormat::Float, _) => for &s in samples { w.write_sample(s)?; },
        (_, 24) => for &s in samples {
            let v = (s as f64 * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
            w.write_sample(v)?;
        },
        _ => for &s in samples {
            let v = (s as f64 * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16;
            w.write_sample(v)?;
        },
    }
    w.finalize()?;
    Ok(())
}

/* ── Names and directories ───────────────────────────────────────── */

/// Reduce an arbitrary file stem to `[A-Za-z0-9._-]`, no leading dots, bounded length.
pub fn sanitize_stem(s: &str) -> String {
    let cleaned: String = s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else if c == ' ' { '_' } else { '-' })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '-' || c == '_').to_string();
    let cleaned: String = cleaned.chars().take(80).collect();
    if cleaned.is_empty() { "clip".into() } else { cleaned }
}

pub fn build_filename(stem: &str, start_ms: u64, end_ms: u64, suffix: Option<&str>) -> String {
    let mut name = format!("{}_{}-{}", sanitize_stem(stem), start_ms, end_ms);
    if let Some(s) = suffix { name.push('_'); name.push_str(&sanitize_stem(s)); }
    name.push_str(".wav");
    name
}

/// First non-existing `name`, `name_2`, `name_3`, ... inside `dir`.
pub fn unique_path(dir: &FsPath, filename: &str) -> PathBuf {
    let first = dir.join(filename);
    if !first.exists() { return first; }
    let p = FsPath::new(filename);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("wav");
    for n in 2.. {
        let c = dir.join(format!("{}_{}.{}", stem, n, ext));
        if !c.exists() { return c; }
    }
    unreachable!()
}

/// `<DAOUCHI_HOME or ~/.daouchi>/exports`.
// Exports land in <DAOUCHI_HOME or ~/.daouchi>/exports.
pub fn exports_dir() -> PathBuf {
    let base = std::env::var_os("DAOUCHI_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".daouchi")))
        .unwrap_or_else(|| PathBuf::from(".daouchi"));
    base.join("exports")
}

/* ── Core export (blocking) ──────────────────────────────────────── */

#[derive(Debug)]
pub struct Exported { pub path: PathBuf, pub filename: String, pub duration_ms: u64, pub sample_rate: u32, pub bits: u16 }

pub fn export_clip(
    src_path: &str, name_stem: &str, start_ms: Option<f64>, end_ms: Option<f64>,
    fx: Option<Fx>, out_dir: &FsPath,
) -> Result<Exported, ApiErr> {
    let dec = decode_to_f32(src_path).map_err(|e| internal(format!("decode failed: {e}")))?;
    let total = dec.samples.len() / dec.channels;
    let (s, e) = frame_range(total, dec.sample_rate, start_ms, end_ms).map_err(bad)?;
    let mut clip = dec.samples[s * dec.channels..e * dec.channels].to_vec();
    if let Some(fx) = fx { fx.apply(&mut clip, dec.sample_rate, dec.channels); }

    let to_ms = |f: usize| (f as f64 * 1000.0 / dec.sample_rate as f64).round() as u64;
    let (s_ms, e_ms) = (to_ms(s), to_ms(e));
    std::fs::create_dir_all(out_dir).map_err(internal)?;
    let filename = build_filename(name_stem, s_ms, e_ms, fx.map(|f| f.name()));
    let path = unique_path(out_dir, &filename);
    write_wav(&path, &clip, dec.sample_rate, dec.channels, dec.bits, dec.is_float).map_err(internal)?;
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or(&filename).to_string();
    let bits = if dec.is_float { 32 } else if dec.bits >= 24 { 24 } else { 16 };
    Ok(Exported { path, filename, duration_ms: to_ms(e - s), sample_rate: dec.sample_rate, bits })
}

/* ── Handlers ────────────────────────────────────────────────────── */

const VALID_STEMS: &[&str] = &["vocals", "drums", "bass", "other"];

async fn export_sound(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<ExportBody>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    if let Some(f) = body.format.as_deref() {
        if !f.eq_ignore_ascii_case("wav") { return Err(bad("only format \"wav\" is supported")); }
    }
    let fx = match body.fx_preset.as_deref() {
        None | Some("") | Some("none") => None,
        Some(p) => Some(Fx::parse(p).ok_or_else(|| bad(format!("unknown fx_preset '{p}'")))?),
    };
    let sound_path: String = {
        let conn = state.pool.get();
        conn.query_row("SELECT path FROM sounds WHERE id = ?", [id], |r| r.get(0))
            .map_err(|_| (StatusCode::NOT_FOUND, "sound not found".to_string()))?
    };
    let base_stem = FsPath::new(&sound_path).file_stem().and_then(|s| s.to_str()).unwrap_or("clip").to_string();

    let (src, name_stem) = match body.stem.as_deref() {
        None => (sound_path.clone(), base_stem),
        Some(st) => {
            if !VALID_STEMS.contains(&st) { return Err(bad(format!("invalid stem '{st}'"))); }
            let p = format!("{}/{}.wav", crate::api::stems::stems_output_dir_pub(&sound_path), st);
            if !FsPath::new(&p).is_file() {
                return Err((StatusCode::NOT_FOUND, format!("stem '{st}' is not available for this sound")));
            }
            (p, format!("{base_stem}_{st}"))
        }
    };

    let (start_ms, end_ms) = (body.start_ms, body.end_ms);
    let ex = tokio::task::spawn_blocking(move || {
        export_clip(&src, &name_stem, start_ms, end_ms, fx, &exports_dir())
    }).await.map_err(internal)??;

    Ok(Json(serde_json::json!({
        "path": ex.path.to_string_lossy(),
        "filename": ex.filename,
        "duration_ms": ex.duration_ms,
        "sample_rate": ex.sample_rate,
        "bits": ex.bits,
    })))
}

/// Resolve `requested` and return it only if it is an existing file inside `dir`.
pub fn path_inside(dir: &FsPath, requested: &str) -> Option<PathBuf> {
    let root = std::fs::canonicalize(dir).ok()?;
    let p = std::fs::canonicalize(requested).ok()?;
    (p.is_file() && p.starts_with(&root)).then_some(p)
}

async fn reveal(Json(body): Json<RevealBody>) -> Result<Json<serde_json::Value>, ApiErr> {
    let p = path_inside(&exports_dir(), &body.path)
        .ok_or_else(|| (StatusCode::FORBIDDEN, "path is not an exported file".to_string()))?;
    reveal_in_file_manager(&p).map_err(internal)?;
    Ok(Json(serde_json::json!({ "success": true })))
}

fn reveal_in_file_manager(p: &FsPath) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    { std::process::Command::new("open").arg("-R").arg(p).spawn()?; }
    #[cfg(target_os = "windows")]
    { std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn()?; }
    #[cfg(all(unix, not(target_os = "macos")))]
    { std::process::Command::new("xdg-open").arg(p.parent().unwrap_or(p)).spawn()?; }
    Ok(())
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/sounds/:id/export", post(export_sound))
        .route("/api/export/reveal", post(reveal))
}

/* ── Tests ───────────────────────────────────────────────────────── */

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("daouchi-export-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_src(path: &FsPath, frames: usize, sr: u32, ch: u16, bits: u16) {
        let spec = hound::WavSpec { channels: ch, sample_rate: sr, bits_per_sample: bits, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for f in 0..frames { for c in 0..ch as usize {
            // distinct value per (frame, channel), exercising low 24-bit bits
            let v = ((f as i64 * 977 + c as i64 * 31) % 2_000_000 - 1_000_000) as i32;
            if bits == 24 { w.write_sample(v).unwrap(); } else { w.write_sample((v / 64) as i16).unwrap(); }
        } }
        w.finalize().unwrap();
    }

    fn read_ints(path: &FsPath) -> (hound::WavSpec, Vec<i32>) {
        let mut r = hound::WavReader::open(path).unwrap();
        let spec = r.spec();
        (spec, r.samples::<i32>().map(|s| s.unwrap()).collect())
    }

    #[test]
    fn sample_exact_cut_24bit_roundtrip() {
        let d = tmp("cut");
        let src = d.join("in.wav");
        write_src(&src, 48_000, 48_000, 2, 24);
        let (_, orig) = read_ints(&src);
        let ex = export_clip(src.to_str().unwrap(), "in", Some(100.0), Some(300.0), None, &d.join("out")).unwrap();
        let (spec, got) = read_ints(&ex.path);
        assert_eq!((spec.bits_per_sample, spec.channels, spec.sample_rate), (24, 2, 48_000));
        assert_eq!(ex.bits, 24);
        assert_eq!(ex.duration_ms, 200);
        // 100 ms @ 48k = frame 4800, 300 ms = frame 14400, 2 channels
        assert_eq!(got.len(), (14_400 - 4_800) * 2);
        assert_eq!(got[..], orig[4_800 * 2..14_400 * 2]);
        assert_eq!(ex.filename, "in_100-300.wav");
    }

    #[test]
    fn sixteen_bit_source_stays_16_and_preserves_rate() {
        let d = tmp("16");
        let src = d.join("in.wav");
        write_src(&src, 22_050, 22_050, 1, 16);
        let (_, orig) = read_ints(&src);
        let ex = export_clip(src.to_str().unwrap(), "in", None, None, None, &d.join("out")).unwrap();
        let (spec, got) = read_ints(&ex.path);
        assert_eq!((spec.bits_per_sample, spec.sample_rate, spec.channels), (16, 22_050, 1));
        assert_eq!(got, orig);
        assert_eq!(ex.duration_ms, 1000);
    }

    #[test]
    fn collisions_get_unique_names() {
        let d = tmp("uniq");
        let src = d.join("in.wav");
        write_src(&src, 4_800, 48_000, 1, 16);
        let out = d.join("out");
        let a = export_clip(src.to_str().unwrap(), "in", None, None, None, &out).unwrap();
        let b = export_clip(src.to_str().unwrap(), "in", None, None, None, &out).unwrap();
        assert_ne!(a.path, b.path);
        assert!(b.filename.ends_with("_2.wav"));
    }

    #[test]
    fn bounds() {
        assert_eq!(frame_range(1000, 1000, None, None), Ok((0, 1000)));
        assert_eq!(frame_range(1000, 1000, Some(100.0), Some(5000.0)), Ok((100, 1000))); // clamped
        assert!(frame_range(1000, 1000, Some(500.0), Some(500.0)).is_err());
        assert!(frame_range(1000, 1000, Some(600.0), Some(500.0)).is_err());
        assert!(frame_range(1000, 1000, Some(1000.0), None).is_err());
        assert!(frame_range(1000, 1000, Some(-1.0), None).is_err());
        assert!(frame_range(1000, 1000, Some(f64::NAN), None).is_err());
    }

    #[test]
    fn fx_allowlist() {
        for ok in ["underwater", "telephone", "clubdrop", "lofi"] { assert!(Fx::parse(ok).is_some()); }
        for bad in ["", "../../etc/passwd", "reverb", "lofi/../x", "Lofi", "underwater.wav"] { assert!(Fx::parse(bad).is_none(), "{bad}"); }
    }

    #[test]
    fn filename_sanitization() {
        let n = build_filename("../../etc/pa ss*wd", 0, 10, Some("lofi"));
        assert!(!n.contains('/') && !n.contains('\\') && !n.starts_with('.'), "{n}");
        assert!(n.ends_with("_0-10_lofi.wav"));
        assert_eq!(sanitize_stem("..."), "clip");
        assert_eq!(sanitize_stem("Kick Drum 01"), "Kick_Drum_01");
        assert!(sanitize_stem(&"a".repeat(500)).len() <= 80);
    }

    #[test]
    fn fx_changes_audio_and_is_named() {
        let d = tmp("fx");
        let src = d.join("in.wav");
        write_src(&src, 4_800, 48_000, 1, 16);
        let (_, orig) = read_ints(&src);
        let ex = export_clip(src.to_str().unwrap(), "in", None, None, Fx::parse("telephone"), &d.join("out")).unwrap();
        assert!(ex.filename.ends_with("_telephone.wav"));
        assert_ne!(read_ints(&ex.path).1, orig);
    }

    #[test]
    fn reveal_only_inside_exports_dir() {
        let d = tmp("reveal");
        let inside = d.join("exports");
        std::fs::create_dir_all(&inside).unwrap();
        let f = inside.join("a.wav");
        std::fs::write(&f, b"x").unwrap();
        let outside = d.join("b.wav");
        std::fs::write(&outside, b"x").unwrap();
        assert!(path_inside(&inside, f.to_str().unwrap()).is_some());
        assert!(path_inside(&inside, outside.to_str().unwrap()).is_none());
        let sneaky = format!("{}/../b.wav", inside.display());
        assert!(path_inside(&inside, &sneaky).is_none());
    }
}
