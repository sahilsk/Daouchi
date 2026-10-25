//! CLAP (HTSAT / laion non-fusion == HF ClapFeatureExtractor) log-mel front-end.
//! n_fft 1024, hop 480, periodic Hann, reflect-center padding, power spectrum,
//! 64-band Slaney mel (50..14000 Hz) via a 513x64 matrix, 10*log10(max(x, 1e-10)).
use realfft::RealFftPlanner;
use std::sync::OnceLock;

pub const N_FFT: usize = 1024;
pub const HOP: usize = 480;
pub const N_BINS: usize = N_FFT / 2 + 1; // 513
pub const N_MELS: usize = 64;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CLIP_SAMPLES: usize = 480_000;
pub const CLIP_FRAMES: usize = CLIP_SAMPLES / HOP + 1; // 1001

static MEL_BYTES: &[u8] = include_bytes!("../../assets/melW_513x64_f32le.bin");

/// Mel matrix stored row-major as [bin][mel] (513 x 64).
fn mel_matrix() -> &'static [f32] {
    static M: OnceLock<Vec<f32>> = OnceLock::new();
    M.get_or_init(|| {
        assert_eq!(MEL_BYTES.len(), N_BINS * N_MELS * 4);
        MEL_BYTES.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
    })
}

fn hann_periodic() -> &'static [f64] {
    static W: OnceLock<Vec<f64>> = OnceLock::new();
    W.get_or_init(|| {
        (0..N_FFT)
            .map(|n| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / N_FFT as f64).cos())
            .collect()
    })
}

fn reflect_index(i: isize, n: usize) -> usize {
    let n = n as isize;
    let mut i = i;
    // single reflection is enough while n > pad; loop for very short inputs
    while i < 0 || i >= n {
        if i < 0 {
            i = -i;
        }
        if i >= n {
            i = 2 * (n - 1) - i;
        }
        if n == 1 {
            return 0;
        }
    }
    i as usize
}

/// Log-mel spectrogram as a row-major [frames][64] buffer; frames = 1 + len/480.
pub fn log_mel(x: &[f32]) -> (Vec<f32>, usize) {
    let n = x.len();
    if n == 0 {
        return (Vec::new(), 0);
    }
    let frames = 1 + n / HOP;
    let pad = (N_FFT / 2) as isize;
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(N_FFT);
    let win = hann_periodic();
    let w = mel_matrix();
    let mut out = vec![0.0f32; frames * N_MELS];
    let mut inbuf = fft.make_input_vec();
    let mut spec = fft.make_output_vec();
    let mut scratch = fft.make_scratch_vec();
    let mut power = vec![0.0f64; N_BINS];
    let mut acc = [0.0f64; N_MELS];
    for f in 0..frames {
        let start = (f * HOP) as isize - pad;
        for k in 0..N_FFT {
            let idx = start + k as isize;
            let s = if idx >= 0 && (idx as usize) < n { x[idx as usize] } else { x[reflect_index(idx, n)] };
            inbuf[k] = s as f64 * win[k];
        }
        fft.process_with_scratch(&mut inbuf, &mut spec, &mut scratch).expect("fft");
        for (p, c) in power.iter_mut().zip(spec.iter()) {
            *p = c.re * c.re + c.im * c.im;
        }
        acc.iter_mut().for_each(|a| *a = 0.0);
        for (b, &p) in power.iter().enumerate() {
            if p == 0.0 {
                continue;
            }
            let wr = &w[b * N_MELS..(b + 1) * N_MELS];
            for m in 0..N_MELS {
                acc[m] += p * wr[m] as f64;
            }
        }
        let row = &mut out[f * N_MELS..(f + 1) * N_MELS];
        for (o, a) in row.iter_mut().zip(acc.iter()) {
            *o = (10.0 * a.max(1e-10).log10()) as f32;
        }
    }
    (out, frames)
}

/// Prepare a 48 kHz mono signal for the model: int16 round trip, repeat-pad or center-crop to 10 s,
/// returns the log-mel `[1001*64]`.
pub fn clip_features(mono48k: &[f32]) -> Vec<f32> {
    let mut x = crate::audio::center_crop(mono48k, CLIP_SAMPLES).to_vec();
    crate::audio::int16_round_trip(&mut x);
    let x = crate::audio::repeat_pad(&x, CLIP_SAMPLES);
    log_mel(&x).0
}
