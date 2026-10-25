//! Deterministic synthetic test signals shared by all golden tests.
//!
//! No audio files live in the repository: every clip is built here from an integer LCG and sums of
//! sines, sweeps, decaying partials, simple one-pole IIR noise and click trains (f64 math), quantised to
//! i16. The reference implementation (PyTorch/transformers/librosa) re-implements this exact generator
//! to produce the numeric golden outputs under `tests/fixtures/`; `generator_parity` pins a few values
//! so a divergence between the two generators is caught directly.
#![allow(dead_code)]

use std::f64::consts::PI;

// ---- primitives -------------------------------------------------------------------------------

/// 64-bit LCG (Knuth MMIX constants); output is the high 32 bits.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }
    pub fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 32) as u32
    }
    /// Uniform in [-1, 1).
    pub fn uni(&mut self) -> f64 {
        self.next_u32() as f64 / 2147483648.0 - 1.0
    }
    pub fn noise(&mut self, n: usize) -> Vec<f64> {
        (0..n).map(|_| self.uni()).collect()
    }
}

/// round-half-up to an index
fn ix(x: f64) -> usize {
    (x + 0.5).floor() as usize
}

fn q16(x: &[f64]) -> Vec<i16> {
    x.iter().map(|&v| (v * 32767.0 + 0.5).floor().clamp(-32768.0, 32767.0) as i16).collect()
}

fn onepole(x: &[f64], a: f64) -> Vec<f64> {
    let mut y = 0.0;
    x.iter()
        .map(|&v| {
            y = a * y + (1.0 - a) * v;
            y
        })
        .collect()
}

fn hann(i: usize, n: usize) -> f64 {
    0.5 - 0.5 * (2.0 * PI * i as f64 / (n as f64 - 1.0)).cos()
}

fn add_at(y: &mut [f64], s: usize, w: &[f64]) {
    let m = w.len().min(y.len().saturating_sub(s));
    for k in 0..m {
        y[s + k] += w[k];
    }
}

fn scale(x: Vec<f64>, g: f64) -> Vec<f64> {
    x.into_iter().map(|v| v * g).collect()
}

fn sil(n: usize) -> Vec<f64> {
    vec![0.0; n]
}

/// Harmonic tone with 5 Hz vibrato (depth `vib` rad) and 50 ms fades.
fn tone(sr: f64, f: f64, n: usize, nh: usize, vib: f64, amp: f64) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let t = i as f64 / sr;
            let ph = 2.0 * PI * f * t + vib * (2.0 * PI * 5.0 * t).sin();
            let mut s = 0.0;
            for k in 0..nh {
                s += (((k + 1) as f64) * ph).sin() / (k + 1) as f64;
            }
            let e = 1.0f64.min((i as f64 / (0.05 * sr)).min((n - i) as f64 / (0.05 * sr)));
            s * e * amp
        })
        .collect()
}

fn chirp(sr: f64, f0: f64, f1: f64, n: usize) -> Vec<f64> {
    let d = n as f64 / sr;
    (0..n)
        .map(|i| {
            let t = i as f64 / sr;
            (2.0 * PI * (f0 * t + (f1 - f0) * t * t / (2.0 * d))).sin()
        })
        .collect()
}

/// Band-limited noise: difference of two one-pole low-passes.
fn bpn(r: &mut Rng, n: usize, a1: f64, a2: f64, g: f64) -> Vec<f64> {
    let x = r.noise(n);
    onepole(&x, a1).iter().zip(onepole(&x, a2)).map(|(a, b)| (a - b) * g).collect()
}

fn lpn(r: &mut Rng, n: usize, a: f64, g: f64) -> Vec<f64> {
    let x = r.noise(n);
    onepole(&x, a).into_iter().map(|v| v * g).collect()
}

// ---- clips ------------------------------------------------------------------------------------

/// Interleaved i16 clip; `f32()` gives `pcm / 32768`.
#[derive(Clone)]
pub struct Clip {
    pub sr: u32,
    pub channels: usize,
    pub pcm: Vec<i16>,
}

impl Clip {
    pub fn f32(&self) -> Vec<f32> {
        self.pcm.iter().map(|&v| v as f32 / 32768.0).collect()
    }
    pub fn mono_f32(&self) -> Vec<f32> {
        daouchi_ai::audio::downmix_mono(&self.f32(), self.channels)
    }
}

fn mono(sr: u32, x: &[f64]) -> Clip {
    Clip { sr, channels: 1, pcm: q16(x) }
}

/// 16 kHz master used by the AST clips (~17 s of tone, sweeps, drums, bowl, noise, clicks).
fn ast_master() -> Vec<i16> {
    let sr = 16000.0;
    let n = |s: f64| ix(s * sr);
    let mut r = Rng::new(3);
    let mut out: Vec<f64> = vec![];
    out.extend(tone(sr, 220.0, n(1.2), 8, 2.0, 0.3));
    out.extend(sil(n(0.5)));
    out.extend(scale(chirp(sr, 200.0, 4000.0, n(1.5)), 0.4));
    out.extend(scale(bpn(&mut r, n(1.0), 0.6, 0.9, 1.0), 0.5));
    let mut d = sil(n(1.5));
    for i in 0..4usize {
        let hn = 4800;
        let w: Vec<f64> = if i % 2 == 0 {
            (0..hn)
                .map(|j| {
                    let k = j as f64 / sr;
                    (2.0 * PI * (55.0 + 120.0 * (-30.0 * k).exp()) * k).sin() * (-12.0 * k).exp()
                })
                .collect()
        } else {
            let u = r.noise(hn);
            (0..hn).map(|j| u[j] * (-20.0 * (j as f64 / sr)).exp() * 0.5).collect()
        };
        add_at(&mut d, i * 5600, &w);
    }
    out.extend(scale(d, 0.8));
    out.extend(sil(n(0.8)));
    let parts = [(0.5, 440.0, 1.2), (0.3, 1170.0, 1.8), (0.2, 2110.0, 2.5), (0.1, 3300.0, 3.5)];
    out.extend((0..n(2.0)).map(|i| {
        let t = i as f64 / sr;
        let mut s = 0.0;
        for (a, f, dc) in parts {
            s += a * (2.0 * PI * f * t).sin() * (-dc * t).exp();
        }
        s * 0.5
    }));
    out.extend(lpn(&mut r, n(1.0), 0.95, 3.0));
    let bn = n(1.0);
    let b = bpn(&mut r, bn, 0.2, 0.7, 1.6);
    out.extend((0..bn).map(|i| {
        let am = 0.5 + 0.5 * (2.0 * PI * 6.0 * (i as f64 / sr)).sin();
        b[i] * am * 0.8
    }));
    let mut c = sil(n(1.0));
    let clen = c.len();
    let click: Vec<f64> = (0..60).map(|k| (1.1 * k as f64).sin() * (-(k as f64) / 10.0).exp() * 0.7).collect();
    let mut s = 0;
    while s < clen - 60 {
        add_at(&mut c, s, &click);
        s += 1600;
    }
    out.extend(c);
    out.extend(tone(sr, 660.0, n(0.8), 4, 0.0, 0.05 * 0.3));
    out.extend(sil(n(0.4)));
    out.extend(scale(chirp(sr, 6000.0, 500.0, n(1.0)), 0.5 * 0.4));
    q16(&out)
}

/// AST clips at 16 kHz: `short` (200 samples), `mid` (3 s), `exact` (163840 samples), `long` (20 s).
pub const AST_CLIPS: [&str; 4] = ["short", "mid", "exact", "long"];

pub fn ast_clip(name: &str) -> Vec<f32> {
    let m = ast_master();
    let cyc = |n: usize| (0..n).map(|i| m[i % m.len()]).collect::<Vec<i16>>();
    let pcm: Vec<i16> = match name {
        "short" => m[35200..35400].to_vec(),
        "mid" => m[..48000].to_vec(),
        "exact" => cyc(163840),
        "long" => cyc(320000),
        _ => panic!("unknown AST clip {name}"),
    };
    pcm.iter().map(|&v| v as f32 / 32768.0).collect()
}

/// CLAP clips (various rates / channel counts; `long_48k` is 26 s for the long-audio paths).
pub const CLAP_CLIPS: [&str; 7] =
    ["noise_burst_48k", "sweep_48k", "chord_short_48k", "stereo_48k", "chirp_44k", "noise_22k", "long_48k"];

pub fn clap_clip(name: &str) -> Clip {
    match name {
        "noise_burst_48k" => {
            let mut r = Rng::new(1234);
            let x: Vec<f64> = (0..192000).map(|i| 0.9 * (-2.5 * (i as f64 / 48000.0)).exp() * r.uni()).collect();
            mono(48000, &x)
        }
        "sweep_48k" => mono(48000, &scale(chirp(48000.0, 80.0, 9000.0, 144000), 0.5)),
        "chord_short_48k" => {
            let n = 19200;
            let x: Vec<f64> = (0..n)
                .map(|i| {
                    let t = i as f64 / 48000.0;
                    0.3 * ((2.0 * PI * 220.0 * t).sin() + (2.0 * PI * 277.0 * t).sin() + (2.0 * PI * 330.0 * t).sin())
                        * hann(i, n)
                })
                .collect();
            mono(48000, &x)
        }
        "stereo_48k" => {
            let n = 144000;
            let l: Vec<f64> = (0..n).map(|i| 0.5 * (2.0 * PI * 440.0 * (i as f64 / 48000.0)).sin()).collect();
            let rr = scale(chirp(48000.0, 200.0, 4000.0, n), 0.4);
            let (lq, rq) = (q16(&l), q16(&rr));
            Clip { sr: 48000, channels: 2, pcm: lq.iter().zip(&rq).flat_map(|(a, b)| [*a, *b]).collect() }
        }
        "chirp_44k" => {
            let n = 132300;
            let mut r = Rng::new(99);
            let c = chirp(44100.0, 100.0, 9000.0, n);
            let x: Vec<f64> = (0..n).map(|i| 0.5 * c[i] + 0.03 * r.uni()).collect();
            mono(44100, &x)
        }
        "noise_22k" => {
            let n = 88200;
            let mut r = Rng::new(7);
            let ph: Vec<f64> = (0..197).map(|_| PI * r.uni()).collect();
            let mut y = vec![0.0f64; n];
            for (k, p) in ph.iter().enumerate() {
                let f = 120.0 + 40.0 * k as f64;
                for (i, v) in y.iter_mut().enumerate() {
                    *v += (2.0 * PI * f * (i as f64 / 22050.0) + p).sin();
                }
            }
            let m = y.iter().fold(0.0f64, |a, v| a.max(v.abs()));
            let x: Vec<f64> = (0..n).map(|i| 0.4 * (y[i] / m) * hann(i, n).sqrt()).collect();
            mono(22050, &x)
        }
        "long_48k" => {
            // sweep, noise burst, chord x10, reversed sweep, noise burst (13 s), then the same block reversed
            let mut base: Vec<i16> = vec![];
            let sweep = clap_clip("sweep_48k").pcm;
            base.extend(&sweep);
            base.extend(clap_clip("noise_burst_48k").pcm);
            let chord = clap_clip("chord_short_48k").pcm;
            for _ in 0..10 {
                base.extend(&chord);
            }
            base.extend(sweep.iter().rev());
            base.extend(clap_clip("noise_burst_48k").pcm);
            let mut pcm = base.clone();
            pcm.extend(base.iter().rev());
            Clip { sr: 48000, channels: 1, pcm }
        }
        _ => panic!("unknown CLAP clip {name}"),
    }
}

pub const TR_SR: u32 = 22050;
const TN: usize = 66150;
pub const TRANSIENT_CLIPS: [&str; 7] =
    ["clicks", "drums", "noiseburst", "clipped", "synth_beat_a", "synth_beat_b", "synth_bowl"];

fn clicks(times: &[f64]) -> Vec<f64> {
    let mut y = sil(TN);
    let w: Vec<f64> = (0..110).map(|k| (0.9 * k as f64).sin() * (-(k as f64) / 20.0).exp()).collect();
    for &t in times {
        add_at(&mut y, ix(t * 22050.0), &w);
    }
    y
}

fn hit(r: &mut Rng, kind: char, n: usize) -> Vec<f64> {
    let k = |i: usize| i as f64 / 22050.0;
    match kind {
        'k' => (0..n)
            .map(|i| (2.0 * PI * (50.0 + 130.0 * (-35.0 * k(i)).exp()) * k(i)).sin() * (-12.0 * k(i)).exp())
            .collect(),
        's' => {
            let u = r.noise(n);
            (0..n)
                .map(|i| {
                    u[i] * (-22.0 * k(i)).exp() * 0.6 + (2.0 * PI * 190.0 * k(i)).sin() * (-28.0 * k(i)).exp() * 0.5
                })
                .collect()
        }
        _ => {
            let u = r.noise(n);
            (0..n)
                .map(|i| {
                    let p = if i == 0 { 0.0 } else { u[i - 1] };
                    (u[i] - p) * (-70.0 * k(i)).exp() * 0.5
                })
                .collect()
        }
    }
}

fn pattern(r: &mut Rng, evs: &[(f64, char)]) -> Vec<f64> {
    let mut y = sil(TN);
    for &(t, kind) in evs {
        let w = hit(r, kind, ix(0.2 * 22050.0));
        let g = if kind == 'h' { 0.5 } else { 1.0 };
        add_at(&mut y, ix(t * 22050.0), &scale(w, g));
    }
    scale(y, 0.8)
}

/// Transient test clips (22.05 kHz mono, 3 s), as f32 in [-1, 1].
pub fn transient_clip(name: &str) -> Vec<f32> {
    let tsr = 22050.0;
    let mut r = Rng::new(0);
    let x: Vec<f64> = match name {
        "clicks" => clicks(&[0.3, 0.8, 1.4, 1.9, 2.2, 2.7]),
        "drums" => {
            let mut y = sil(TN);
            for (t, kind) in [(0.1, 'k'), (0.6, 's'), (1.1, 'k'), (1.35, 'k'), (1.6, 's'), (2.1, 'k'), (2.6, 's')] {
                let n = ix(0.25 * tsr);
                let w: Vec<f64> = if kind == 'k' {
                    (0..n)
                        .map(|i| {
                            let k = i as f64 / tsr;
                            (2.0 * PI * (60.0 + 120.0 * (-30.0 * k).exp()) * k).sin() * (-14.0 * k).exp()
                        })
                        .collect()
                } else {
                    let u = r.noise(n);
                    (0..n).map(|i| u[i] * (-18.0 * (i as f64 / tsr)).exp() * 0.6).collect()
                };
                add_at(&mut y, ix(t * tsr), &w);
            }
            scale(y, 0.8)
        }
        "noiseburst" => {
            // the drum clip consumes RNG state first (mirrors the reference generator's single Rng(0))
            let _ = transient_rng_after_drums(&mut r);
            let u = r.noise(TN);
            (0..TN)
                .map(|i| {
                    let t = i as f64 / tsr;
                    if t < 0.5 { 0.0 } else { u[i] * (-3.0 * t).exp() * 0.5 }
                })
                .collect()
        }
        "clipped" => clicks(&[0.4, 1.0, 1.8, 2.4]).into_iter().map(|v| (v * 8.0).clamp(-1.0, 1.0)).collect(),
        "synth_beat_a" => {
            let mut r2 = Rng::new(7);
            let a = [0.5, 0.78, 0.9, 1.02, 1.28, 1.42, 1.5, 1.78, 1.9, 2.02, 2.28, 2.42, 2.5, 2.78, 2.9];
            let evs: Vec<(f64, char)> = a.iter().cloned().zip("khshkshkhskhskh".chars()).collect();
            pattern(&mut r2, &evs)
        }
        "synth_beat_b" => {
            let mut r2 = Rng::new(7);
            // beat B continues the Rng(7) stream after beat A
            let a = [0.5, 0.78, 0.9, 1.02, 1.28, 1.42, 1.5, 1.78, 1.9, 2.02, 2.28, 2.42, 2.5, 2.78, 2.9];
            let ea: Vec<(f64, char)> = a.iter().cloned().zip("khshkshkhskhskh".chars()).collect();
            let _ = pattern(&mut r2, &ea);
            let b = [0.16, 0.28, 0.42, 0.53, 0.81, 1.04, 1.32, 1.44, 1.58, 2.09, 2.23, 2.35, 2.6, 2.88];
            let evs: Vec<(f64, char)> = b.iter().cloned().zip("hkshkshskhskhs".chars()).collect();
            pattern(&mut r2, &evs)
        }
        "synth_bowl" => {
            let parts = [(1.0, 0.7, 2.2), (2.32, 0.45, 3.0), (2.9, 0.35, 3.6), (3.76, 0.3, 4.5), (5.4, 0.2, 6.0)];
            let mut y = sil(TN);
            for t0 in [0.14, 1.0, 1.9] {
                let s = ix(t0 * tsr);
                let w: Vec<f64> = (0..TN - s)
                    .map(|i| {
                        let k = i as f64 / tsr;
                        let mut acc = 0.0;
                        for (m, a, d) in parts {
                            acc += a * (2.0 * PI * m * 420.0 * k).sin() * (-d * k).exp();
                        }
                        acc * 1.0f64.min(k * 800.0)
                    })
                    .collect();
                add_at(&mut y, s, &w);
            }
            scale(y, 0.35)
        }
        _ => panic!("unknown transient clip {name}"),
    };
    q16(&x).iter().map(|&v| v as f32 / 32768.0).collect()
}

/// Advance `r` exactly as the "drums" clip does (7 events; snares draw `ix(0.25*22050)` samples each).
fn transient_rng_after_drums(r: &mut Rng) {
    for kind in ['k', 's', 'k', 'k', 's', 'k', 's'] {
        if kind == 's' {
            r.noise(ix(0.25 * 22050.0));
        }
    }
}

// ---- parity -----------------------------------------------------------------------------------

/// (len, sum of i16 values, sum of |values|, [pcm at a few fixed indices]) of a clip's interleaved i16 PCM.
pub fn fingerprint(pcm: &[i16]) -> (usize, i64, i64, [i16; 4]) {
    let sum: i64 = pcm.iter().map(|&v| v as i64).sum();
    let abs: i64 = pcm.iter().map(|&v| (v as i64).abs()).sum();
    let n = pcm.len();
    (n, sum, abs, [pcm[n / 7], pcm[n / 3], pcm[n / 2], pcm[n - 1 - n / 9]])
}

pub fn ast_pcm(name: &str) -> Vec<i16> {
    ast_clip(name).iter().map(|&v| (v * 32768.0).round() as i16).collect()
}
pub fn clap_pcm(name: &str) -> Vec<i16> {
    clap_clip(name).pcm
}
pub fn transient_pcm(name: &str) -> Vec<i16> {
    transient_clip(name).iter().map(|&v| (v * 32768.0).round() as i16).collect()
}

/// Little-endian f32 fixture reader.
pub fn read_f32(path: &std::path::Path) -> Vec<f32> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

pub fn fixture_dir(sub: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(sub)
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

/// Minimal tracing subscriber that records WARN-level messages, so tests can assert that the CoreML
/// execution provider did not silently fall back to CPU ("CoreML unavailable ... falling back to CPU").
pub mod warnings {
    use std::sync::{Mutex, OnceLock};
    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    static LOG: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    fn log() -> &'static Mutex<Vec<String>> {
        LOG.get_or_init(|| Mutex::new(Vec::new()))
    }

    struct Capture;
    struct Msg(String);
    impl Visit for Msg {
        fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
            if f.name() == "message" {
                self.0 = format!("{v:?}");
            }
        }
    }
    impl Subscriber for Capture {
        fn enabled(&self, m: &Metadata<'_>) -> bool {
            *m.level() <= Level::WARN
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, e: &Event<'_>) {
            let mut m = Msg(String::new());
            e.record(&mut m);
            log().lock().unwrap().push(m.0);
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// Install the capturing subscriber (idempotent).
    pub fn install() {
        let _ = tracing::subscriber::set_global_default(Capture);
    }
    pub fn take() -> Vec<String> {
        std::mem::take(&mut *log().lock().unwrap())
    }
    pub fn assert_no_coreml_fallback() {
        let w = take();
        assert!(!w.iter().any(|m| m.contains("CoreML unavailable")), "CoreML fell back to CPU: {w:?}");
    }
}
