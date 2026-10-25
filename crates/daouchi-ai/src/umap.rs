//! UMAP for the acoustic radar: 512-d CLAP embeddings -> 2-d coordinates.
//!
//! Pure Rust, no BLAS / system libs. Pipeline:
//!   1. drop rows that are not `expected_dim`-d or are non-finite, L2-normalise the rest
//!   2. exact brute-force kNN (rayon, 16-query register tiling) on the unit sphere
//!   3. fuzzy simplicial set (smooth-kNN sigma/rho search, probabilistic-union symmetrisation)
//!   4. PCA (power iteration) initial layout
//!   5. seeded, single-threaded SGD layout (same update rule as umap-learn)
//!
//! Why in-house rather than the `umap-rs` crate: `umap-rs` 0.4.x draws negative samples from the
//! unseeded `rand::rng()` and runs lock-free parallel SGD, so the output is not reproducible, and
//! it pulls in ndarray 0.17 next to our 0.16. The requirement here is deterministic output for a
//! fixed seed, so the layout stage is implemented here (about 100 lines).
//!
//! Output coordinates are centred and uniformly scaled so the largest |coordinate| is 1.

use anyhow::{bail, Result};
use rayon::prelude::*;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct UmapParams {
    pub n_neighbors: usize,
    pub min_dist: f32,
    pub seed: u64,
    /// Rows whose length differs are dropped (non-512-d rows are dropped).
    pub expected_dim: usize,
    /// `None` = umap-learn default (500 epochs for <=10k points, else 200).
    pub n_epochs: Option<usize>,
}

impl Default for UmapParams {
    fn default() -> Self {
        Self { n_neighbors: 15, min_dist: 0.1, seed: 42, expected_dim: 512, n_epochs: None }
    }
}

/// UMAP is not meaningful below this many points; fall back to a PCA layout instead.
pub const MIN_POINTS_FOR_UMAP: usize = 16;

// ───────────────────────── RNG ─────────────────────────

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        let mut r = Rng(seed ^ 0x9E37_79B9_7F4A_7C15);
        for _ in 0..4 {
            r.next_u64();
        }
        r
    }
    #[inline]
    fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    #[inline]
    fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() >> 32) * n as u64 >> 32) as usize
    }
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
}

// ───────────────────────── public API ─────────────────────────

/// Project vectors to 2-d. Returns `(id, x, y)` for every *valid* input row (invalid rows are
/// dropped), in input order. `progress` receives values in 0..=1.
pub fn project_2d(
    ids: &[i64],
    vectors: &[Vec<f32>],
    params: &UmapParams,
    progress: impl Fn(f32),
) -> Result<Vec<(i64, f32, f32)>> {
    if ids.len() != vectors.len() {
        bail!("ids ({}) and vectors ({}) differ in length", ids.len(), vectors.len());
    }
    // 1. validate + normalise into a flat buffer
    let dim = params.expected_dim;
    if dim == 0 {
        bail!("expected_dim must be > 0");
    }
    let mut kept_ids = Vec::new();
    let mut flat: Vec<f32> = Vec::new();
    for (id, v) in ids.iter().zip(vectors) {
        if v.len() != dim || v.iter().any(|x| !x.is_finite()) {
            continue;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm == 0.0 {
            continue;
        }
        kept_ids.push(*id);
        flat.extend(v.iter().map(|x| x / norm));
    }
    let n = kept_ids.len();
    progress(0.0);
    if n == 0 {
        return Ok(vec![]);
    }

    let mut rng = Rng::new(params.seed);
    let init = pca_init(&flat, n, dim, &mut rng);
    let coords = if n < MIN_POINTS_FOR_UMAP {
        init // too few points for a meaningful manifold: PCA layout
    } else {
        let k = params.n_neighbors.clamp(2, n - 1);
        let (idx, dist) = knn(&flat, n, dim, k);
        progress(0.25);
        let edges = fuzzy_graph(&idx, &dist, n, k);
        let n_epochs = params.n_epochs.unwrap_or(if n <= 10_000 { 500 } else { 200 });
        let (a, b) = fit_ab(params.min_dist, 1.0);
        optimize(init, &edges, n, n_epochs, a, b, &mut rng, |f| progress(0.3 + 0.7 * f))
    };
    progress(1.0);
    let coords = normalize(coords);
    Ok(kept_ids.into_iter().zip(coords).map(|(id, (x, y))| (id, x, y)).collect())
}

/// Place a new point among already-projected ones: similarity-weighted average of the `k` most
/// cosine-similar existing points' coordinates. `(0,0)` if nothing usable.
pub fn place_new(
    existing_ids: &[i64],
    existing_xy: &[(f32, f32)],
    existing_vectors: &[Vec<f32>],
    new_vector: &[f32],
    k: usize,
) -> (f32, f32) {
    debug_assert_eq!(existing_ids.len(), existing_xy.len());
    let nn = new_vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if nn == 0.0 || new_vector.iter().any(|x| !x.is_finite()) {
        return (0.0, 0.0);
    }
    let mut sims: Vec<(f32, usize)> = existing_vectors
        .iter()
        .enumerate()
        .take(existing_xy.len())
        .filter(|(_, v)| v.len() == new_vector.len() && v.iter().all(|x| x.is_finite()))
        .filter_map(|(i, v)| {
            let vn = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if vn == 0.0 {
                return None;
            }
            let dot: f32 = v.iter().zip(new_vector).map(|(a, b)| a * b).sum();
            Some((dot / (vn * nn), i))
        })
        .collect();
    if sims.is_empty() {
        return (0.0, 0.0);
    }
    sims.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    sims.truncate(k.max(1));
    // euclidean distance between unit vectors, inverse-distance weights
    let (mut sx, mut sy, mut sw) = (0.0f64, 0.0f64, 0.0f64);
    for (cos, i) in sims {
        let d = (2.0 - 2.0 * cos).max(0.0).sqrt();
        let w = 1.0 / (d as f64 + 1e-3);
        sx += w * existing_xy[i].0 as f64;
        sy += w * existing_xy[i].1 as f64;
        sw += w;
    }
    ((sx / sw) as f32, (sy / sw) as f32)
}

// ───────────────────────── kNN ─────────────────────────

#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    for (ca, cb) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
        for l in 0..8 {
            acc[l] += ca[l] * cb[l];
        }
    }
    let rem = a.len() - a.len() % 8;
    let mut s: f32 = acc.iter().sum();
    for i in rem..a.len() {
        s += a[i] * b[i];
    }
    s
}

const QTILE: usize = 16;

/// Exact kNN (excluding self) on unit vectors. Returns row-major (n*k) indices and euclidean
/// distances, each row sorted ascending (ties broken by index -> deterministic).
fn knn(x: &[f32], n: usize, dim: usize, k: usize) -> (Vec<u32>, Vec<f32>) {
    let mut idx = vec![0u32; n * k];
    let mut dst = vec![0f32; n * k];
    idx.par_chunks_mut(QTILE * k).zip(dst.par_chunks_mut(QTILE * k)).enumerate().for_each(
        |(t, (idx_c, dst_c))| {
            let q0 = t * QTILE;
            let nq = idx_c.len() / k;
            // best[q] = sorted (sim desc) top-k list
            let mut best: Vec<Vec<(f32, u32)>> = vec![Vec::with_capacity(k + 1); nq];
            for j in 0..n {
                let row = &x[j * dim..(j + 1) * dim];
                for q in 0..nq {
                    let qi = q0 + q;
                    if qi == j {
                        continue;
                    }
                    let s = dot(&x[qi * dim..(qi + 1) * dim], row);
                    let b = &mut best[q];
                    if b.len() == k && s <= b[k - 1].0 {
                        continue;
                    }
                    let pos = b.partition_point(|&(bs, _)| bs >= s);
                    b.insert(pos, (s, j as u32));
                    b.truncate(k);
                }
            }
            for q in 0..nq {
                for (c, &(s, j)) in best[q].iter().enumerate() {
                    idx_c[q * k + c] = j;
                    dst_c[q * k + c] = (2.0 - 2.0 * s).max(0.0).sqrt();
                }
            }
        },
    );
    (idx, dst)
}

// ───────────────────────── fuzzy simplicial set ─────────────────────────

/// Undirected weighted edges (i<j).
struct Edges {
    i: Vec<u32>,
    j: Vec<u32>,
    w: Vec<f32>,
}

fn fuzzy_graph(idx: &[u32], dist: &[f32], n: usize, k: usize) -> Edges {
    let target = (k as f32).log2();
    // directed memberships
    let rows: Vec<Vec<f32>> = (0..n)
        .into_par_iter()
        .map(|i| {
            let d = &dist[i * k..(i + 1) * k];
            let rho = d.iter().copied().find(|&v| v > 0.0).unwrap_or(0.0);
            let (mut lo, mut hi, mut mid) = (0.0f32, f32::INFINITY, 1.0f32);
            for _ in 0..64 {
                let psum: f32 =
                    d.iter().map(|&v| { let t = v - rho; if t > 0.0 { (-t / mid).exp() } else { 1.0 } }).sum();
                if (psum - target).abs() < 1e-5 {
                    break;
                }
                if psum > target {
                    hi = mid;
                    mid = (lo + hi) / 2.0;
                } else {
                    lo = mid;
                    mid = if hi.is_infinite() { mid * 2.0 } else { (lo + hi) / 2.0 };
                }
            }
            let mean_d = d.iter().sum::<f32>() / k as f32;
            let floor = 1e-3 * mean_d;
            let sigma = if mean_d > 0.0 { mid.max(floor) } else { mid.max(1e-6) };
            d.iter()
                .map(|&v| { let t = v - rho; if t <= 0.0 { 1.0 } else { (-t / sigma).exp() } })
                .collect()
        })
        .collect();

    let mut map: HashMap<(u32, u32), (f32, f32)> = HashMap::with_capacity(n * k);
    for i in 0..n {
        for c in 0..k {
            let j = idx[i * k + c];
            let w = rows[i][c];
            let (key, fwd) = if (i as u32) < j { ((i as u32, j), true) } else { ((j, i as u32), false) };
            let e = map.entry(key).or_insert((0.0, 0.0));
            if fwd { e.0 = w } else { e.1 = w }
        }
    }
    let mut items: Vec<((u32, u32), f32)> =
        map.into_iter().map(|(k, (a, b))| (k, a + b - a * b)).filter(|(_, w)| *w > 0.0).collect();
    items.sort_by_key(|(k, _)| *k); // deterministic order
    Edges {
        i: items.iter().map(|(k, _)| k.0).collect(),
        j: items.iter().map(|(k, _)| k.1).collect(),
        w: items.iter().map(|(_, w)| *w).collect(),
    }
}

// ───────────────────────── a, b curve fit ─────────────────────────

/// Fit `1/(1+a x^(2b))` to the piecewise target used by umap-learn (Levenberg-Marquardt).
fn fit_ab(min_dist: f32, spread: f32) -> (f32, f32) {
    let xs: Vec<f64> = (1..=300).map(|i| i as f64 * 3.0 * spread as f64 / 300.0).collect();
    let ys: Vec<f64> = xs
        .iter()
        .map(|&x| if x <= min_dist as f64 { 1.0 } else { (-(x - min_dist as f64) / spread as f64).exp() })
        .collect();
    let f = |a: f64, b: f64, x: f64| 1.0 / (1.0 + a * x.powf(2.0 * b));
    let cost = |a: f64, b: f64| xs.iter().zip(&ys).map(|(&x, &y)| (f(a, b, x) - y).powi(2)).sum::<f64>();
    let (mut a, mut b, mut lam) = (1.5f64, 1.0f64, 1e-3f64);
    let mut c = cost(a, b);
    for _ in 0..200 {
        let (mut jtj, mut jtr) = ([[0.0f64; 2]; 2], [0.0f64; 2]);
        for (&x, &y) in xs.iter().zip(&ys) {
            let p = x.powf(2.0 * b);
            let d = 1.0 + a * p;
            let r = f(a, b, x) - y;
            let da = -p / (d * d);
            let db = -a * p * (2.0 * x.ln()) / (d * d);
            jtj[0][0] += da * da; jtj[0][1] += da * db; jtj[1][1] += db * db;
            jtr[0] += da * r; jtr[1] += db * r;
        }
        jtj[1][0] = jtj[0][1];
        let m00 = jtj[0][0] * (1.0 + lam);
        let m11 = jtj[1][1] * (1.0 + lam);
        let det = m00 * m11 - jtj[0][1] * jtj[1][0];
        if det.abs() < 1e-18 { break; }
        let da = -(m11 * jtr[0] - jtj[0][1] * jtr[1]) / det;
        let db = -(m00 * jtr[1] - jtj[1][0] * jtr[0]) / det;
        let (na, nb) = ((a + da).max(1e-3), (b + db).max(1e-3));
        let nc = cost(na, nb);
        if nc < c {
            let conv = (c - nc) < 1e-12;
            a = na; b = nb; c = nc; lam = (lam * 0.5).max(1e-9);
            if conv { break; }
        } else {
            lam *= 4.0;
            if lam > 1e9 { break; }
        }
    }
    (a as f32, b as f32)
}

// ───────────────────────── PCA init ─────────────────────────

/// Top-2 principal components via power iteration on the (implicit) covariance, scaled to
/// max |coord| = 10 like umap-learn's init, plus tiny seeded jitter so duplicates separate.
fn pca_init(x: &[f32], n: usize, dim: usize, rng: &mut Rng) -> Vec<(f32, f32)> {
    let mut mean = vec![0.0f64; dim];
    for r in 0..n {
        for d in 0..dim { mean[d] += x[r * dim + d] as f64; }
    }
    mean.iter_mut().for_each(|m| *m /= n as f64);
    let mean: Vec<f32> = mean.into_iter().map(|m| m as f32).collect();
    let centered: Vec<f32> = (0..n * dim).map(|i| x[i] - mean[i % dim]).collect();

    // y = X^T (X v)
    let cov_mul = |v: &[f32]| -> Vec<f32> {
        let proj: Vec<f32> = (0..n).into_par_iter().map(|r| dot(&centered[r * dim..(r + 1) * dim], v)).collect();
        let mut out = vec![0.0f64; dim];
        for r in 0..n {
            let p = proj[r] as f64;
            for d in 0..dim { out[d] += p * centered[r * dim + d] as f64; }
        }
        out.into_iter().map(|v| v as f32).collect()
    };
    let mut comps: Vec<Vec<f32>> = Vec::new();
    for _ in 0..2 {
        let mut v: Vec<f32> = (0..dim).map(|_| rng.unit() + 1e-3).collect();
        for _ in 0..60 {
            let mut w = cov_mul(&v);
            for c in &comps {
                let p = dot(&w, c);
                for d in 0..dim { w[d] -= p * c[d]; }
            }
            let nrm = w.iter().map(|a| a * a).sum::<f32>().sqrt();
            if nrm < 1e-20 { break; }
            w.iter_mut().for_each(|a| *a /= nrm);
            v = w;
        }
        let nrm = v.iter().map(|a| a * a).sum::<f32>().sqrt();
        if nrm > 0.0 { v.iter_mut().for_each(|a| *a /= nrm); }
        comps.push(v);
    }
    let mut pts: Vec<(f32, f32)> = (0..n)
        .map(|r| {
            let row = &centered[r * dim..(r + 1) * dim];
            (dot(row, &comps[0]), dot(row, &comps[1]))
        })
        .collect();
    let m = pts.iter().fold(0.0f32, |m, p| m.max(p.0.abs()).max(p.1.abs()));
    let s = if m > 0.0 { 10.0 / m } else { 1.0 };
    for p in pts.iter_mut() {
        p.0 = p.0 * s + 1e-3 * rng.unit();
        p.1 = p.1 * s + 1e-3 * rng.unit();
    }
    pts
}

// ───────────────────────── SGD layout ─────────────────────────

#[inline]
fn clip4(v: f32) -> f32 { v.clamp(-4.0, 4.0) }

#[allow(clippy::too_many_arguments)]
fn optimize(
    init: Vec<(f32, f32)>,
    e: &Edges,
    n: usize,
    n_epochs: usize,
    a: f32,
    b: f32,
    rng: &mut Rng,
    progress: impl Fn(f32),
) -> Vec<(f32, f32)> {
    let mut y: Vec<f32> = init.iter().flat_map(|p| [p.0, p.1]).collect();
    let wmax = e.w.iter().copied().fold(0.0f32, f32::max);
    if wmax <= 0.0 || e.w.is_empty() {
        return init;
    }
    // drop negligible edges like umap-learn
    let keep: Vec<usize> = (0..e.w.len()).filter(|&t| e.w[t] >= wmax / n_epochs as f32).collect();
    let neg_rate = 5.0f32;
    let eps_per_sample: Vec<f32> = keep.iter().map(|&t| wmax / e.w[t]).collect();
    let mut next_sample = eps_per_sample.clone();
    let mut next_neg: Vec<f32> = eps_per_sample.iter().map(|v| v / neg_rate).collect();
    let mut neg_done = vec![0f32; keep.len()];
    let _ = &mut neg_done;
    let report_every = (n_epochs / 50).max(1);

    for epoch in 0..n_epochs {
        let alpha = 1.0 - epoch as f32 / n_epochs as f32;
        for (s, &t) in keep.iter().enumerate() {
            if next_sample[s] > epoch as f32 { continue; }
            let (i, j) = (e.i[t] as usize, e.j[t] as usize);
            // attraction
            let (dx, dy) = (y[2 * i] - y[2 * j], y[2 * i + 1] - y[2 * j + 1]);
            let d2 = dx * dx + dy * dy;
            if d2 > 0.0 {
                let g = -2.0 * a * b * d2.powf(b - 1.0) / (a * d2.powf(b) + 1.0);
                let (gx, gy) = (clip4(g * dx), clip4(g * dy));
                y[2 * i] += gx * alpha; y[2 * i + 1] += gy * alpha;
                y[2 * j] -= gx * alpha; y[2 * j + 1] -= gy * alpha;
            }
            next_sample[s] += eps_per_sample[s];
            // repulsion
            let n_neg = ((epoch as f32 - next_neg[s]) / (eps_per_sample[s] / neg_rate)).max(0.0) as usize + 1;
            let n_neg = if next_neg[s] <= epoch as f32 { n_neg } else { 0 };
            for _ in 0..n_neg {
                let k = rng.below(n);
                if k == i { continue; }
                let (dx, dy) = (y[2 * i] - y[2 * k], y[2 * i + 1] - y[2 * k + 1]);
                let d2 = dx * dx + dy * dy;
                let (gx, gy) = if d2 > 0.0 {
                    let g = 2.0 * b / ((0.001 + d2) * (a * d2.powf(b) + 1.0));
                    (clip4(g * dx), clip4(g * dy))
                } else {
                    (4.0, 4.0)
                };
                y[2 * i] += gx * alpha; y[2 * i + 1] += gy * alpha;
            }
            next_neg[s] += n_neg as f32 * (eps_per_sample[s] / neg_rate);
        }
        if epoch % report_every == 0 { progress(epoch as f32 / n_epochs as f32); }
    }
    y.chunks_exact(2).map(|c| (c[0], c[1])).collect()
}

// ───────────────────────── output normalisation ─────────────────────────

fn normalize(pts: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    if pts.is_empty() { return pts; }
    let (mut x0, mut x1, mut y0, mut y1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for &(x, y) in &pts {
        x0 = x0.min(x); x1 = x1.max(x); y0 = y0.min(y); y1 = y1.max(y);
    }
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let half = ((x1 - x0).max(y1 - y0)) / 2.0;
    let s = if half > 0.0 && half.is_finite() { 1.0 / half } else { 0.0 };
    pts.into_iter().map(|(x, y)| (((x - cx) * s).clamp(-1.0, 1.0), ((y - cy) * s).clamp(-1.0, 1.0))).collect()
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn gauss(rng: &mut Rng) -> f32 {
        let u1 = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64 + 1e-12;
        let u2 = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32
    }

    /// `clusters` Gaussian blobs in `dim`-d. Returns (ids, vectors, labels).
    fn blobs(n: usize, clusters: usize, dim: usize, seed: u64) -> (Vec<i64>, Vec<Vec<f32>>, Vec<usize>) {
        let mut rng = Rng::new(seed);
        let centers: Vec<Vec<f32>> = (0..clusters)
            .map(|_| {
                let v: Vec<f32> = (0..dim).map(|_| gauss(&mut rng)).collect();
                let nrm = v.iter().map(|a| a * a).sum::<f32>().sqrt();
                v.into_iter().map(|a| a / nrm).collect()
            })
            .collect();
        let mut vecs = Vec::new();
        let mut labels = Vec::new();
        for i in 0..n {
            let c = i % clusters;
            vecs.push(centers[c].iter().map(|m| m + 0.02 * gauss(&mut rng)).collect());
            labels.push(c);
        }
        ((0..n as i64).collect(), vecs, labels)
    }

    fn trustworthiness(high: &[Vec<f32>], low: &[(i64, f32, f32)], k: usize) -> f64 {
        let n = high.len();
        let unit: Vec<Vec<f32>> = high.iter().map(|v| {
            let nr = v.iter().map(|a| a * a).sum::<f32>().sqrt();
            v.iter().map(|a| a / nr).collect()
        }).collect();
        let mut t = 0.0f64;
        for i in 0..n {
            let mut hd: Vec<(f32, usize)> = (0..n).filter(|&j| j != i)
                .map(|j| (-dot(&unit[i], &unit[j]), j)).collect();
            hd.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut rank = vec![0usize; n];
            for (r, &(_, j)) in hd.iter().enumerate() { rank[j] = r + 1; }
            let mut ld: Vec<(f32, usize)> = (0..n).filter(|&j| j != i)
                .map(|j| ((low[i].1 - low[j].1).powi(2) + (low[i].2 - low[j].2).powi(2), j)).collect();
            ld.sort_by(|a, b| a.0.total_cmp(&b.0));
            for &(_, j) in ld.iter().take(k) {
                if rank[j] > k { t += (rank[j] - k) as f64; }
            }
        }
        1.0 - 2.0 / (n as f64 * k as f64 * (2.0 * n as f64 - 3.0 * k as f64 - 1.0)) * t
    }

    #[test]
    fn umap_separates_clusters() {
        let (ids, vecs, labels) = blobs(2000, 5, 512, 7);
        let t = Instant::now();
        let out = project_2d(&ids, &vecs, &UmapParams::default(), |_| {}).unwrap();
        println!("umap 2k x 512: {:.2?}", t.elapsed());
        assert_eq!(out.len(), 2000);
        assert!(out.iter().all(|p| p.1.abs() <= 1.0 && p.2.abs() <= 1.0 && p.1.is_finite()));
        // kNN label purity in 2-d
        let mut agree = 0usize;
        for i in 0..out.len() {
            let mut d: Vec<(f32, usize)> = (0..out.len()).filter(|&j| j != i)
                .map(|j| ((out[i].1 - out[j].1).powi(2) + (out[i].2 - out[j].2).powi(2), j)).collect();
            d.sort_by(|a, b| a.0.total_cmp(&b.0));
            agree += d.iter().take(10).filter(|(_, j)| labels[*j] == labels[i]).count();
        }
        let purity = agree as f64 / (out.len() * 10) as f64;
        // centroid separation vs spread
        let mut cen = vec![(0.0f64, 0.0f64, 0usize); 5];
        for (p, &l) in out.iter().zip(&labels) { cen[l].0 += p.1 as f64; cen[l].1 += p.2 as f64; cen[l].2 += 1; }
        let cen: Vec<(f64, f64)> = cen.iter().map(|c| (c.0 / c.2 as f64, c.1 / c.2 as f64)).collect();
        let intra = out.iter().zip(&labels).map(|(p, &l)| ((p.1 as f64 - cen[l].0).powi(2) + (p.2 as f64 - cen[l].1).powi(2)).sqrt()).sum::<f64>() / out.len() as f64;
        let mut min_inter = f64::MAX;
        for a in 0..5 { for b in a + 1..5 {
            min_inter = min_inter.min(((cen[a].0 - cen[b].0).powi(2) + (cen[a].1 - cen[b].1).powi(2)).sqrt());
        } }
        let tw = trustworthiness(&vecs, &out, 10);
        println!("purity@10={purity:.4} intra={intra:.4} min_inter={min_inter:.4} ratio={:.1} trustworthiness@10={tw:.4}", min_inter / intra);
        assert!(purity > 0.99, "purity {purity}");
        assert!(min_inter / intra > 4.0);
        assert!(tw > 0.85, "trustworthiness {tw}");
    }

    #[test]
    fn umap_deterministic() {
        let (ids, vecs, _) = blobs(300, 3, 512, 3);
        let a = project_2d(&ids, &vecs, &UmapParams::default(), |_| {}).unwrap();
        let b = project_2d(&ids, &vecs, &UmapParams::default(), |_| {}).unwrap();
        assert_eq!(a, b);
        let c = project_2d(&ids, &vecs, &UmapParams { seed: 1, ..Default::default() }, |_| {}).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn umap_edge_cases() {
        let (mut ids, mut vecs, _) = blobs(40, 2, 512, 5);
        // invalid rows dropped
        vecs[3][10] = f32::NAN;
        vecs[4].truncate(100);
        vecs[5] = vec![0.0; 512];
        ids.push(999); vecs.push(vecs[0].clone()); // exact duplicate
        ids.push(1000); vecs.push(vecs[0].clone());
        let out = project_2d(&ids, &vecs, &UmapParams::default(), |_| {}).unwrap();
        assert_eq!(out.len(), 40 + 2 - 3);
        assert!(!out.iter().any(|p| p.0 == 3 || p.0 == 4 || p.0 == 5));
        assert!(out.iter().all(|p| p.1.is_finite() && p.2.is_finite()));
        // few points -> PCA fallback, still returns something sensible
        let few = project_2d(&ids[..8], &vecs[..8], &UmapParams::default(), |_| {}).unwrap();
        assert_eq!(few.len(), 8 - 3);
        assert!(project_2d(&[], &[], &UmapParams::default(), |_| {}).unwrap().is_empty());
        let one = project_2d(&[1], &[vecs[0].clone()], &UmapParams::default(), |_| {}).unwrap();
        assert_eq!(one, vec![(1, 0.0, 0.0)]);
        // all-identical points
        let same: Vec<Vec<f32>> = (0..30).map(|_| vecs[0].clone()).collect();
        let sids: Vec<i64> = (0..30).collect();
        let o = project_2d(&sids, &same, &UmapParams::default(), |_| {}).unwrap();
        assert!(o.iter().all(|p| p.1.is_finite() && p.2.is_finite()));
    }

    #[test]
    fn umap_fit_ab_matches_umap_learn() {
        let (a, b) = fit_ab(0.1, 1.0);
        assert!((a - 1.577).abs() < 0.03 && (b - 0.895).abs() < 0.02, "a={a} b={b}");
    }

    #[test]
    fn umap_place_new_lands_in_own_cluster() {
        let (ids, vecs, labels) = blobs(600, 3, 512, 11);
        let out = project_2d(&ids, &vecs, &UmapParams::default(), |_| {}).unwrap();
        let xy: Vec<(f32, f32)> = out.iter().map(|p| (p.1, p.2)).collect();
        // a fresh sample from cluster 1's centre (reuse an existing vector perturbed)
        let q: Vec<f32> = vecs[1].iter().enumerate().map(|(i, v)| v + 0.001 * ((i % 7) as f32 - 3.0)).collect();
        let (x, y) = place_new(&ids, &xy, &vecs, &q, 5);
        let c = labels[1];
        let (mut cx, mut cy, mut m) = (0.0, 0.0, 0.0);
        for (p, &l) in xy.iter().zip(&labels) { if l == c { cx += p.0; cy += p.1; m += 1.0; } }
        let own = ((x - cx / m).powi(2) + (y - cy / m).powi(2)).sqrt();
        assert!(own < 0.2, "placed {own} away from own cluster centroid");
    }

    #[test]
    #[ignore = "timing; run with --release -- --ignored --nocapture"]
    fn umap_timing_10k() {
        let (ids, vecs, _) = blobs(10_000, 8, 512, 9);
        let t = Instant::now();
        let out = project_2d(&ids, &vecs, &UmapParams::default(), |_| {}).unwrap();
        println!("umap 10k x 512: {:.2?}", t.elapsed());
        assert_eq!(out.len(), 10_000);
    }
}
