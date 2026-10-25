//! Transient (onset) detection: pure-Rust port of `librosa.onset.onset_detect` defaults
//! (sr 22050, n_fft 2048, hop 512, 128 Slaney mels, power->dB, spectral flux, peak_pick, backtrack=False).
use anyhow::Result;
use realfft::RealFftPlanner;
use std::path::Path;

const SR: u32 = 22050;
const N_FFT: usize = 2048;
const HOP: usize = 512;
const N_MELS: usize = 128;
const MAX_TICKS: usize = 80;

/// Detect onsets in a file; returns ascending fractions of duration in 0..1 (<= 80 ticks).
pub fn detect(path: &Path) -> Result<Vec<f32>> {
    let d = crate::audio::decode_file(path)?;
    let mono = d.to_mono();
    Ok(detect_samples(&mono, d.sample_rate))
}

/// Detect onsets in mono samples at `sample_rate`; returns fractions of duration.
pub fn detect_samples(mono: &[f32], sample_rate: u32) -> Vec<f32> {
    if mono.is_empty() || sample_rate == 0 {
        return vec![];
    }
    let duration = mono.len() as f64 / sample_rate as f64;
    let clean: Vec<f32> = mono.iter().map(|v| if v.is_finite() { *v } else { 0.0 }).collect();
    let y = if sample_rate == SR {
        clean
    } else {
        match crate::audio::resample_mono(&clean, sample_rate, SR) {
            Ok(v) => v,
            Err(_) => return vec![],
        }
    };
    let env = onset_strength(&y);
    let frames = onset_frames(&env);
    let mut fracs: Vec<f32> = frames
        .iter()
        .map(|&f| f as f64 * HOP as f64 / SR as f64 / duration)
        .filter(|t| (0.0..=1.0).contains(t))
        .map(|t| t as f32)
        .collect();
    if fracs.len() > MAX_TICKS {
        let step = fracs.len() as f64 / MAX_TICKS as f64;
        fracs = (0..MAX_TICKS).map(|i| fracs[(i as f64 * step) as usize]).collect();
    }
    fracs
}

fn hz_to_mel(f: f64) -> f64 {
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = (6.4f64).ln() / 27.0;
    if f >= min_log_hz { min_log_mel + (f / min_log_hz).ln() / logstep } else { f / f_sp }
}
fn mel_to_hz(m: f64) -> f64 {
    let f_sp = 200.0 / 3.0;
    let min_log_mel = 1000.0 / f_sp;
    let logstep = (6.4f64).ln() / 27.0;
    if m >= min_log_mel { 1000.0 * (logstep * (m - min_log_mel)).exp() } else { m * f_sp }
}

/// Slaney-normalised mel filterbank as (first_bin, weights) per mel.
fn mel_filters() -> Vec<(usize, Vec<f32>)> {
    let nb = N_FFT / 2 + 1;
    let fft_f: Vec<f64> = (0..nb).map(|i| i as f64 * SR as f64 / N_FFT as f64).collect();
    let (lo, hi) = (hz_to_mel(0.0), hz_to_mel(SR as f64 / 2.0));
    let mel_f: Vec<f64> =
        (0..N_MELS + 2).map(|i| mel_to_hz(lo + (hi - lo) * i as f64 / (N_MELS + 1) as f64)).collect();
    (0..N_MELS)
        .map(|i| {
            let enorm = 2.0 / (mel_f[i + 2] - mel_f[i]);
            let w: Vec<f64> = fft_f
                .iter()
                .map(|&f| {
                    let lower = (f - mel_f[i]) / (mel_f[i + 1] - mel_f[i]);
                    let upper = (mel_f[i + 2] - f) / (mel_f[i + 2] - mel_f[i + 1]);
                    lower.min(upper).max(0.0) * enorm
                })
                .collect();
            let first = w.iter().position(|&x| x > 0.0).unwrap_or(0);
            let last = w.iter().rposition(|&x| x > 0.0).map(|p| p + 1).unwrap_or(first);
            (first, w[first..last].iter().map(|&x| x as f32).collect())
        })
        .collect()
}

/// librosa.onset.onset_strength defaults (lag 1, max_size 1, mean aggregate, centered).
fn onset_strength(y: &[f32]) -> Vec<f32> {
    let n_frames = 1 + y.len() / HOP;
    let pad = N_FFT / 2;
    let mut padded = vec![0f32; y.len() + 2 * pad];
    padded[pad..pad + y.len()].copy_from_slice(y);
    let window: Vec<f32> = (0..N_FFT)
        .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / N_FFT as f64).cos()) as f32)
        .collect();
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(N_FFT);
    let filters = mel_filters();
    let mut inbuf = fft.make_input_vec();
    let mut spec = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();
    let mut db = vec![0f32; n_frames * N_MELS];
    let mut power = vec![0f32; N_FFT / 2 + 1];
    for t in 0..n_frames {
        let s = t * HOP;
        for i in 0..N_FFT {
            inbuf[i] = padded.get(s + i).copied().unwrap_or(0.0) * window[i];
        }
        if fft.process_with_scratch(&mut inbuf, &mut spec, &mut scratch).is_err() {
            return vec![0.0; n_frames];
        }
        for (p, c) in power.iter_mut().zip(spec.iter()) {
            *p = c.re * c.re + c.im * c.im;
        }
        for (m, (first, w)) in filters.iter().enumerate() {
            let mut acc = 0f32;
            for (k, wk) in w.iter().enumerate() {
                acc += wk * power[first + k];
            }
            db[t * N_MELS + m] = 10.0 * acc.max(1e-10).log10();
        }
    }
    let max = db.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    for v in db.iter_mut() {
        *v = v.max(max - 80.0);
    }
    // flux: mean over mels of half-wave rectified lag-1 difference, left-padded by 1 + n_fft/(2*hop)
    let lead = 1 + N_FFT / (2 * HOP);
    let mut env = vec![0f32; n_frames];
    for t in 1..n_frames {
        let mut s = 0f32;
        for m in 0..N_MELS {
            s += (db[t * N_MELS + m] - db[(t - 1) * N_MELS + m]).max(0.0);
        }
        let dst = t - 1 + lead;
        if dst < n_frames {
            env[dst] = s / N_MELS as f32;
        }
    }
    env
}

/// librosa.onset.onset_detect peak picking (defaults for sr=22050, hop=512, backtrack=False).
fn onset_frames(env: &[f32]) -> Vec<usize> {
    let mut x = env.to_vec();
    let mn = x.iter().cloned().fold(f32::INFINITY, f32::min);
    if !mn.is_finite() {
        return vec![];
    }
    x.iter_mut().for_each(|v| *v -= mn);
    let mx = x.iter().cloned().fold(0f32, f32::max);
    if mx <= 0.0 {
        return vec![];
    }
    x.iter_mut().for_each(|v| *v /= mx);

    let f = |secs: f64| (secs * SR as f64 / HOP as f64).floor() as usize;
    let (pre_max, post_max) = (f(0.03), f(0.0) + 1);
    let (pre_avg, post_avg) = (f(0.10), f(0.10) + 1);
    let wait = f(0.03);
    let delta = 0.07f32;
    let n = x.len() as isize;
    let max_len = (pre_max + post_max) as isize;
    let max_origin = (0.5 * (pre_max as f64 - post_max as f64)).ceil() as isize;
    let avg_len = (pre_avg + post_avg) as isize;
    let avg_origin = (0.5 * (pre_avg as f64 - post_avg as f64)).ceil() as isize;

    let mut peaks: Vec<usize> = vec![];
    let mut last: isize = isize::MIN / 2;
    for i in 0..n {
        let lo = i - max_len / 2 - max_origin;
        let hi = i + (max_len - 1 - max_len / 2) - max_origin;
        let mut m = 0f32; // constant mode, cval 0
        for j in lo..=hi {
            if j >= 0 && j < n {
                m = m.max(x[j as usize]);
            }
        }
        let v = x[i as usize];
        if v != m || v <= 0.0 {
            continue;
        }
        let lo = i - avg_len / 2 - avg_origin;
        let hi = i + (avg_len - 1 - avg_len / 2) - avg_origin;
        let mut s = 0f32;
        for j in lo..=hi {
            s += x[j.clamp(0, n - 1) as usize];
        }
        let avg = s / avg_len as f32;
        if v >= avg + delta && i > last + wait as isize {
            peaks.push(i as usize);
            last = i;
        }
    }
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clicks(sr: u32, secs: f32, times: &[f32]) -> Vec<f32> {
        let mut y = vec![0f32; (sr as f32 * secs) as usize];
        for &t in times {
            let s = (t * sr as f32) as usize;
            for k in 0..(sr as usize / 200) {
                if s + k < y.len() {
                    y[s + k] = ((k as f32) * 0.9).sin() * (-(k as f32) / 20.0).exp();
                }
            }
        }
        y
    }

    #[test]
    fn silence_and_empty() {
        assert!(detect_samples(&[], 44100).is_empty());
        assert!(detect_samples(&vec![0.0; 44100], 44100).is_empty());
        assert!(detect_samples(&[0.1; 10], 44100).len() <= 1);
        assert!(detect_samples(&vec![0.5; 44100], 44100).len() <= 1); // DC step at t=0 only
        assert!(detect_samples(&[f32::NAN; 5000], 8000).is_empty());
    }

    #[test]
    fn click_train_all_rates() {
        let times = [0.5f32, 1.0, 1.5, 2.0, 2.5];
        for sr in [8000u32, 22050, 44100, 48000, 96000] {
            let y = clicks(sr, 3.0, &times);
            let f = detect_samples(&y, sr);
            assert_eq!(f.len(), 5, "sr {sr}: {f:?}");
            for (a, t) in f.iter().zip(times) {
                assert!((a * 3.0 - t).abs() < 0.05, "sr {sr}: {a} vs {t}");
            }
        }
    }

    #[test]
    fn capped_at_80_and_ascending() {
        let times: Vec<f32> = (0..200).map(|i| 0.1 + i as f32 * 0.1).collect();
        let f = detect_samples(&clicks(22050, 21.0, &times), 22050);
        assert!(f.len() <= 80 && f.len() > 40);
        assert!(f.windows(2).all(|w| w[0] < w[1]) && f.iter().all(|v| (0.0..=1.0).contains(v)));
    }
}
