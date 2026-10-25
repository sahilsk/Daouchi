//! The Rust synthetic-signal generator must match the reference generator (fingerprints below were
//! printed by the Python implementation). A divergence would invalidate every golden fixture.
mod common;
use common::*;

type Fp = (usize, i64, i64, [i16; 4]);

fn check(kind: &str, name: &str, got: Fp, want: Fp) {
    assert_eq!(got.0, want.0, "{kind} {name}: length");
    // libm vs numpy transcendental functions can differ by 1 ulp, flipping rare samples by 1 LSB
    let tol = (want.0 as i64 / 5000).max(2);
    assert!((got.1 - want.1).abs() <= tol, "{kind} {name}: sum {} vs {}", got.1, want.1);
    assert!((got.2 - want.2).abs() <= tol, "{kind} {name}: abs-sum {} vs {}", got.2, want.2);
    for (a, b) in got.3.iter().zip(want.3.iter()) {
        assert!((a - b).abs() <= 1, "{kind} {name}: samples {:?} vs {:?}", got.3, want.3);
    }
}

#[test]
fn ast_generator_matches_reference() {
    let want: [(&str, Fp); 4] = [
        ("short", (200, -29909, 1669409, [13065, -13071, -8802, 3789])),
        ("mid", (48000, 176425, 308352911, [-435, 0, 0, -10477])),
        ("exact", (163840, 4087543, 689219651, [0, -3123, 1410, 13142])),
        ("long", (320000, 5079426, 1213851478, [6780, 3336, -770, 1817])),
    ];
    for (n, w) in want {
        check("ast", n, fingerprint(&ast_pcm(n)), w);
    }
}

#[test]
fn clap_generator_matches_reference() {
    let want: [(&str, Fp); 7] = [
        ("noise_burst_48k", (192000, -375154, 284438322, [-4221, 603, -113, 3])),
        ("sweep_48k", (144000, 1530000, 1501875616, [14110, -14189, 0, 1883])),
        ("chord_short_48k", (19200, 43, 94374603, [-3904, 3386, 5778, 2248])),
        ("stereo_48k", (288000, 476307, 2703503705, [-6742, 0, 0, -3060])),
        ("chirp_44k", (132300, 1302102, 1380805702, [14834, 14066, 200, -11749])),
        ("noise_22k", (88200, -25, 150503807, [-696, -510, -3913, 898])),
        ("long_48k", (1728000, 4620244, 9032747812, [-60, -4185, -1, 1225])),
    ];
    for (n, w) in want {
        check("clap", n, fingerprint(&clap_pcm(n)), w);
    }
}

#[test]
fn transient_generator_matches_reference() {
    let want: [(&str, Fp); 7] = [
        ("clicks", (66150, 203220, 2468904, [0, 0, 0, 0])),
        ("drums", (66150, 2342175, 130560929, [0, 0, 3046, 3835])),
        ("noiseburst", (66150, -120855, 13404595, [0, -226, 21, 5])),
        ("clipped", (66150, 199124, 6142868, [0, 0, 0, 0])),
        ("synth_beat_a", (66150, 2794618, 178436032, [0, -1322, -5358, 192])),
        ("synth_beat_b", (66150, 1723175, 162513125, [-10439, -517, 5624, 55])),
        ("synth_bowl", (66150, -10665, 160413419, [5351, 1183, 1342, -566])),
    ];
    for (n, w) in want {
        check("transient", n, fingerprint(&transient_pcm(n)), w);
    }
}

#[test]
fn rng_is_the_documented_lcg() {
    let mut r = Rng::new(0);
    // state1 = 1442695040888963407 -> high 32 bits
    assert_eq!(r.next_u32(), (1442695040888963407u64 >> 32) as u32);
    let mut a = Rng::new(42);
    let mut b = Rng::new(42);
    assert!((0..100).all(|_| a.next_u32() == b.next_u32()));
}
