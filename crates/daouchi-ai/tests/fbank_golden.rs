//! AST fbank front-end vs HF `ASTFeatureExtractor` (torchaudio kaldi fbank). Fixtures hold every 8th
//! normalised frame (128 frames x 128 mel) of the first 1024-frame window. Clips are synthesized in code.
mod common;
use common::*;
use daouchi_ai::fbank::{ast_features, MAX_FRAMES, NUM_MEL};

#[test]
fn fbank_matches_hf_extractor() {
    let dir = fixture_dir("ast");
    for name in AST_CLIPS {
        let x = ast_clip(name);
        let got = ast_features(&x);
        assert_eq!(got.len(), MAX_FRAMES * NUM_MEL);
        let want = read_f32(&dir.join(format!("{name}.feat8.f32")));
        assert_eq!(want.len(), MAX_FRAMES / 8 * NUM_MEL, "{name}: fixture size");
        let mut maxd = 0f32;
        for (r, row) in want.chunks_exact(NUM_MEL).enumerate() {
            let g = &got[r * 8 * NUM_MEL..(r * 8 + 1) * NUM_MEL];
            for (a, b) in g.iter().zip(row) {
                maxd = maxd.max((a - b).abs());
            }
        }
        eprintln!("fbank {name:6} ({:6} samples): max |diff| = {maxd:.2e}", x.len());
        assert!(maxd < 2e-3, "{name}: max diff {maxd}");
    }
}
