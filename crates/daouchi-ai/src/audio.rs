//! Audio front-end shared by the engines: decode (symphonia), mono downmix, resample (rubato),
//! int16 round trip, and windowing helpers. Pure Rust, no model runtime needed.
use anyhow::{anyhow, Context, Result};
use rubato::{
    calculate_cutoff, FftFixedIn, Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decoded audio, interleaved f32 in [-1, 1].
#[derive(Debug, Clone)]
pub struct Decoded {
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

impl Decoded {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }
    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / self.sample_rate.max(1) as f64
    }
    /// Mono downmix by averaging channels.
    pub fn to_mono(&self) -> Vec<f32> {
        downmix_mono(&self.samples, self.channels)
    }
}

/// Decode wav/mp3/m4a/flac/aiff/... into interleaved f32.
pub fn decode_file(path: &Path) -> Result<Decoded> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| anyhow!("probe {}: {e}", path.display()))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("codec {}: {e}", path.display()))?;

    let mut out: Vec<f32> = Vec::new();
    let mut rate = track.codec_params.sample_rate.unwrap_or(0);
    let mut channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(0);
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(anyhow!("read {}: {e}", path.display())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = *buf.spec();
                rate = spec.rate;
                channels = spec.channels.count();
                if sbuf.as_ref().map_or(true, |s| s.capacity() < buf.capacity() * channels) {
                    sbuf = Some(SampleBuffer::<f32>::new(buf.capacity() as u64, spec));
                }
                let sb = sbuf.as_mut().unwrap();
                sb.copy_interleaved_ref(buf);
                out.extend_from_slice(sb.samples());
            }
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue, // skip corrupt packet
            Err(e) => return Err(anyhow!("decode {}: {e}", path.display())),
        }
    }
    if rate == 0 || channels == 0 || out.is_empty() {
        return Err(anyhow!("no decodable audio in {}", path.display()));
    }
    Ok(Decoded { sample_rate: rate, channels, samples: out })
}

/// Average interleaved channels into mono.
pub fn downmix_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let inv = 1.0 / channels as f32;
    interleaved.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * inv).collect()
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// High-quality band-limited resample of mono audio. Rational ratios with small reduced terms
/// (44.1k/48k/96k/22.05k... <-> 48k) use rubato's FFT polyphase resampler (~1e-6 accurate);
/// anything else falls back to a 256-tap windowed-sinc with cubic interpolation.
/// Output length is `round(len * to / from)`.
pub fn resample_mono(input: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    if from == to || input.is_empty() {
        return Ok(input.to_vec());
    }
    let ratio = to as f64 / from as f64;
    let want = (input.len() as f64 * ratio).round() as usize;
    let g = gcd(from, to);
    if from / g <= 2048 && to / g <= 2048 {
        if let Ok(mut rs) = FftFixedIn::<f32>::new(from as usize, to as usize, 1024, 2, 1) {
            let delay = rs.output_delay();
                    let mut out: Vec<f32> = Vec::with_capacity(want + delay + 4096);
            let mut pos = 0usize;
            let mut buf: Vec<f32> = Vec::new();
            while out.len() < want + delay {
                let need = rs.input_frames_next();
                buf.clear();
                buf.resize(need, 0.0);
                let n = need.min(input.len().saturating_sub(pos));
                buf[..n].copy_from_slice(&input[pos.min(input.len())..pos.min(input.len()) + n]);
                pos += need;
                let res = rs.process(&[&buf[..]], None).map_err(|e| anyhow!("resample: {e}"))?;
                out.extend_from_slice(&res[0]);
            }
            let mut out: Vec<f32> = out.drain(delay..).collect();
            out.truncate(want);
            return Ok(out);
        }
    }
    let sinc_len = 256;
    let window = WindowFunction::BlackmanHarris2;
    let params = SincInterpolationParameters {
        sinc_len,
        f_cutoff: calculate_cutoff(sinc_len, window),
        oversampling_factor: 256,
        interpolation: SincInterpolationType::Cubic,
        window,
    };
    const CHUNK: usize = 4096;
    let mut rs = SincFixedIn::<f32>::new(ratio, 1.0, params, CHUNK, 1)
        .map_err(|e| anyhow!("resampler init: {e}"))?;
    let mut out: Vec<f32> = Vec::with_capacity(want + CHUNK * 2);
    let mut pos = 0usize;
    let mut buf = vec![0.0f32; CHUNK];
    while out.len() < want {
        let need = rs.input_frames_next();
        buf.resize(need, 0.0);
        let end = (pos + need).min(input.len());
        let n = end.saturating_sub(pos);
        buf[..n].copy_from_slice(&input[pos.min(input.len())..end]);
        buf[n..].iter_mut().for_each(|v| *v = 0.0); // zero-flush the tail
        pos += need;
        let res = rs.process(&[&buf[..]], None).map_err(|e| anyhow!("resample: {e}"))?;
        out.extend_from_slice(&res[0]);
    }
    out.truncate(want);
    Ok(out)
}

/// Decode a file to mono f32 at `target_rate`.
pub fn load_mono(path: &Path, target_rate: u32) -> Result<Vec<f32>> {
    let d = decode_file(path)?;
    let mono = d.to_mono();
    resample_mono(&mono, d.sample_rate, target_rate)
}

/// Mimic laion-clap `int16_to_float32(float32_to_int16(x))`: clip, truncate toward zero, rescale.
pub fn int16_round_trip(x: &mut [f32]) {
    for v in x.iter_mut() {
        let q = (v.clamp(-1.0, 1.0) * 32767.0).trunc() as i16;
        *v = q as f32 / 32767.0;
    }
}

/// Shorter than `target`: tile floor(target/len) times, then zero-pad (HF "repeatpad").
/// Longer: returned unchanged (use [`center_crop`]).
pub fn repeat_pad(x: &[f32], target: usize) -> Vec<f32> {
    if x.is_empty() {
        return vec![0.0; target];
    }
    if x.len() >= target {
        return x.to_vec();
    }
    let reps = target / x.len();
    let mut out = Vec::with_capacity(target);
    for _ in 0..reps {
        out.extend_from_slice(x);
    }
    out.resize(target, 0.0);
    out
}

/// Deterministic center crop of `target` samples (whole slice if shorter).
pub fn center_crop(x: &[f32], target: usize) -> &[f32] {
    if x.len() <= target {
        return x;
    }
    let start = (x.len() - target) / 2;
    &x[start..start + target]
}

/// Non-overlapping windows of `win` samples; a trailing remainder shorter than `win` is dropped,
/// except when the whole signal is shorter than `win` (then a single short window is returned).
pub fn chunk_windows(x: &[f32], win: usize) -> Vec<&[f32]> {
    if x.len() <= win {
        return vec![x];
    }
    x.chunks_exact(win).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_pad_tiles_then_zero_pads() {
        let p = repeat_pad(&[1.0, 2.0, 3.0], 8);
        assert_eq!(p, vec![1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 0.0, 0.0]);
        assert_eq!(repeat_pad(&[1.0], 3), vec![1.0, 1.0, 1.0]);
    }

    #[test]
    fn center_crop_is_centered() {
        let v: Vec<f32> = (0..10).map(|i| i as f32).collect();
        assert_eq!(center_crop(&v, 4), &[3.0, 4.0, 5.0, 6.0]);
        assert_eq!(center_crop(&v, 20).len(), 10);
    }

    #[test]
    fn chunks_drop_remainder() {
        let v = vec![0.0f32; 25];
        assert_eq!(chunk_windows(&v, 10).len(), 2);
        assert_eq!(chunk_windows(&v[..5], 10).len(), 1);
    }

    #[test]
    fn int16_round_trip_truncates() {
        let mut x = [0.5f32, -0.5, 2.0, -3.0, 0.00001];
        int16_round_trip(&mut x);
        assert_eq!(x[0], 16383.0 / 32767.0);
        assert_eq!(x[1], -16383.0 / 32767.0);
        assert_eq!(x[2], 1.0);
        assert_eq!(x[3], -1.0);
        assert_eq!(x[4], 0.0);
    }

    #[test]
    fn downmix_averages() {
        assert_eq!(downmix_mono(&[1.0, 3.0, -1.0, 1.0], 2), vec![2.0, 0.0]);
    }

    #[test]
    fn resample_sine_44100_to_48000() {
        let f = 1000.0f32;
        let x: Vec<f32> = (0..44100).map(|i| ((2.0 * std::f64::consts::PI * f as f64 * i as f64 / 44100.0).sin() * 0.5) as f32).collect();
        let y = resample_mono(&x, 44100, 48000).unwrap();
        assert_eq!(y.len(), 48000);
        // compare away from the edges to the ideal sine
        let mut maxd = 0.0f32;
        for i in 2000..46000 {
            let want = ((2.0 * std::f64::consts::PI * f as f64 * i as f64 / 48000.0).sin() * 0.5) as f32;
            maxd = maxd.max((y[i] - want).abs());
        }
        assert!(maxd < 1e-4, "max diff {maxd}");
    }

    #[test]
    fn resample_downsample_length() {
        let x = vec![0.1f32; 96000];
        assert_eq!(resample_mono(&x, 96000, 48000).unwrap().len(), 48000);
    }
}
