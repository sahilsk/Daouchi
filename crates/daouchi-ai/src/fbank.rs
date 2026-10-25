//! Kaldi-style fbank for the Audio Spectrogram Transformer (matches HF `ASTFeatureExtractor`
//! with torchaudio: window "hanning" (symmetric), 128 HTK-mel bins 20-8000 Hz, no 2^15 scale).
//!
//! Pipeline: frames (400 / hop 160, snip_edges) -> remove DC -> preemphasis 0.97 -> symmetric Hann
//! -> zero-pad to 512 -> rfft power (Nyquist dropped) -> mel -> ln(max(e, f32 eps)) ->
//! pad/truncate to 1024 frames (zeros AFTER the log) -> (x + 4.2677393) / (4.5689974 * 2).

use realfft::RealFftPlanner;
use std::sync::OnceLock;

pub const SAMPLE_RATE: usize = 16_000;
pub const FRAME_LEN: usize = 400;
pub const FRAME_SHIFT: usize = 160;
pub const FFT_LEN: usize = 512;
pub const NUM_MEL: usize = 128;
pub const MAX_FRAMES: usize = 1024;
/// Samples covered by one 1024-frame window (10.24 s -> 163_840 samples incl. frame overhang).
pub const WINDOW_SAMPLES: usize = (MAX_FRAMES - 1) * FRAME_SHIFT + FRAME_LEN;
pub const MEAN: f32 = -4.2677393;
pub const STD: f32 = 4.5689974;
const LOG_FLOOR: f64 = 1.1920929e-7;
const PREEMPH: f64 = 0.97;
const MEL_LOW: f64 = 20.0;
const MEL_HIGH: f64 = 8000.0;

fn hz_to_mel(f: f64) -> f64 {
    1127.0 * (1.0 + f / 700.0).ln()
}

struct Tables {
    window: [f64; FRAME_LEN],
    /// NUM_MEL rows x (FFT_LEN/2) columns (Nyquist bin excluded).
    mel: Vec<f64>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut window = [0f64; FRAME_LEN];
        for (k, w) in window.iter_mut().enumerate() {
            *w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / (FRAME_LEN as f64 - 1.0)).cos();
        }
        let nyq = FFT_LEN / 2;
        let (mlo, mhi) = (hz_to_mel(MEL_LOW), hz_to_mel(MEL_HIGH));
        let d = (mhi - mlo) / (NUM_MEL as f64 + 1.0);
        let mut mel = vec![0f64; NUM_MEL * nyq];
        for i in 0..NUM_MEL {
            let (l, c, r) = (mlo + i as f64 * d, mlo + (i as f64 + 1.0) * d, mlo + (i as f64 + 2.0) * d);
            for k in 0..nyq {
                let m = hz_to_mel(k as f64 * SAMPLE_RATE as f64 / FFT_LEN as f64);
                let v = ((m - l) / (c - l)).min((r - m) / (r - c)).max(0.0);
                mel[i * nyq + k] = v;
            }
        }
        Tables { window, mel }
    })
}

/// Raw log-mel frames, row-major `[n_frames][128]`. Input shorter than 400 samples is zero-padded to
/// one frame (the HF/torchaudio reference raises an error there). Returns `n_frames` as well.
pub fn log_mel(samples: &[f32]) -> (Vec<f32>, usize) {
    let t = tables();
    let padded;
    let x: &[f32] = if samples.len() < FRAME_LEN {
        let mut v = samples.to_vec();
        v.resize(FRAME_LEN, 0.0);
        padded = v;
        &padded
    } else {
        samples
    };
    let n = 1 + (x.len() - FRAME_LEN) / FRAME_SHIFT;
    let mut planner = RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(FFT_LEN);
    let mut inbuf = fft.make_input_vec();
    let mut spec = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();
    let nyq = FFT_LEN / 2;
    let mut out = vec![0f32; n * NUM_MEL];
    let mut frame = [0f64; FRAME_LEN];
    let mut power = vec![0f64; nyq];
    for f in 0..n {
        let s = &x[f * FRAME_SHIFT..f * FRAME_SHIFT + FRAME_LEN];
        let mean = s.iter().map(|&v| v as f64).sum::<f64>() / FRAME_LEN as f64;
        for (d, &v) in frame.iter_mut().zip(s) {
            *d = v as f64 - mean;
        }
        inbuf.iter_mut().for_each(|v| *v = 0.0);
        // preemphasis with x[-1] := x[0]; applied from the back so we can do it in place
        for j in (1..FRAME_LEN).rev() {
            inbuf[j] = (frame[j] - PREEMPH * frame[j - 1]) * t.window[j];
        }
        inbuf[0] = (frame[0] - PREEMPH * frame[0]) * t.window[0];
        fft.process_with_scratch(&mut inbuf, &mut spec, &mut scratch).expect("fft");
        for k in 0..nyq {
            power[k] = spec[k].re * spec[k].re + spec[k].im * spec[k].im;
        }
        for m in 0..NUM_MEL {
            let row = &t.mel[m * nyq..(m + 1) * nyq];
            let e: f64 = row.iter().zip(&power).map(|(a, b)| a * b).sum();
            out[f * NUM_MEL + m] = e.max(LOG_FLOOR).ln() as f32;
        }
    }
    (out, n)
}

/// Normalised model input `[1024 * 128]` (row-major, frames x mel) for one clip/window.
pub fn ast_features(samples: &[f32]) -> Vec<f32> {
    let (lm, n) = log_mel(samples);
    let mut out = vec![0f32; MAX_FRAMES * NUM_MEL];
    let keep = n.min(MAX_FRAMES);
    out[..keep * NUM_MEL].copy_from_slice(&lm[..keep * NUM_MEL]);
    let denom = STD * 2.0;
    for v in out.iter_mut() {
        *v = (*v - MEAN) / denom;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_and_padding_value() {
        let f = ast_features(&[0.0; 100]);
        assert_eq!(f.len(), MAX_FRAMES * NUM_MEL);
        // silent: log floor in row 0; zero padding rows normalise to -MEAN/(2 STD)
        let pad = -MEAN / (2.0 * STD);
        assert!((f[NUM_MEL] - pad).abs() < 1e-6);
        assert!(f[0] < f[NUM_MEL]);
    }

    #[test]
    fn long_input_truncates() {
        let x = vec![0.1f32; 16000 * 30];
        let f = ast_features(&x);
        assert_eq!(f.len(), MAX_FRAMES * NUM_MEL);
    }
}
