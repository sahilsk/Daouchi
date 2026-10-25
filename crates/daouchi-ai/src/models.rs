//! Model manifests, on-demand download (resumable, sha256-verified) and status.
use crate::home::DaouchiHome;
use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ModelStatus {
    Missing,
    Partial,
    Ready,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub model: String,
    pub file: String,
    pub file_index: usize,
    pub file_count: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

const CLAP_JSON: &str = include_str!("../assets/manifests/clap.json");
const AST_JSON: &str = include_str!("../assets/manifests/ast.json");

/// Built-in manifest by name (`clap`, `ast`).
pub fn builtin_manifest(name: &str) -> Option<Manifest> {
    let s = match name {
        "clap" => CLAP_JSON,
        "ast" => AST_JSON,
        _ => return None,
    };
    Some(serde_json::from_str(s).expect("built-in manifest is valid"))
}

pub struct ModelManager {
    home: DaouchiHome,
    client: reqwest::Client,
}

fn is_safe_rel(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty() && path.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 { break; }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

impl ModelManager {
    pub fn new(home: DaouchiHome) -> Self {
        Self { home, client: reqwest::Client::new() }
    }

    pub fn home(&self) -> &DaouchiHome { &self.home }

    /// Manifest on disk (`models/<name>/manifest.json`, e.g. preseeded) wins over the built-in.
    pub fn manifest(&self, name: &str) -> Result<Manifest> {
        if !is_safe_rel(name) { bail!("invalid model name {name:?}"); }
        let p = self.home.model_dir(name).join("manifest.json");
        if p.is_file() {
            let m: Manifest = serde_json::from_slice(&std::fs::read(&p)?)
                .with_context(|| format!("parse {}", p.display()))?;
            if m.name != name { bail!("manifest {} names model {:?}, expected {name:?}", p.display(), m.name); }
            return Ok(m);
        }
        builtin_manifest(name).ok_or_else(|| anyhow!("unknown model {name:?} and no manifest at {}", p.display()))
    }

    pub fn file_path(&self, name: &str, rel: &str) -> PathBuf {
        self.home.model_dir(name).join(rel)
    }

    fn file_ok(&self, name: &str, f: &ManifestFile) -> bool {
        is_safe_rel(&f.path) && file_matches(&self.file_path(name, &f.path), f)
    }

    /// Verifies every file's size and sha256.
    pub fn status(&self, name: &str) -> Result<ModelStatus> {
        let m = self.manifest(name)?;
        let ok = m.files.iter().filter(|f| self.file_ok(name, f)).count();
        Ok(if ok == m.files.len() { ModelStatus::Ready } else if ok == 0 && !self.any_present(name, &m) { ModelStatus::Missing } else { ModelStatus::Partial })
    }

    fn any_present(&self, name: &str, m: &Manifest) -> bool {
        m.files.iter().any(|f| {
            is_safe_rel(&f.path) && {
                let p = self.file_path(name, &f.path);
                p.exists() || p.with_extension(part_ext(&p)).exists()
            }
        })
    }

    /// Ensure all files are present and valid; downloads what is missing. Returns the model dir.
    pub async fn ensure<F>(&self, name: &str, mut progress: F) -> Result<PathBuf>
    where
        F: FnMut(Progress) + Send,
    {
        let m = self.manifest(name)?;
        let dir = self.home.model_dir(name);
        tokio::fs::create_dir_all(&dir).await?;
        let total: u64 = m.files.iter().map(|f| f.size).sum();
        let mut done: u64 = 0;
        let n = m.files.len();
        for (i, f) in m.files.iter().enumerate() {
            if !is_safe_rel(&f.path) { bail!("unsafe path in manifest: {}", f.path); }
            let ok = { let f2 = f.clone(); let p = self.file_path(name, &f.path); tokio::task::spawn_blocking(move || file_matches(&p, &f2)).await? };
            let mk = |bytes_done: u64| Progress { model: name.into(), file: f.path.clone(), file_index: i, file_count: n, bytes_done, bytes_total: total };
            if ok {
                done += f.size;
                progress(mk(done));
                continue;
            }
            let dest = self.file_path(name, &f.path);
            self.download(f, &dest, |b| progress(mk(done + b))).await
                .with_context(|| format!("downloading {} for model {name}", f.path))?;
            done += f.size;
            progress(mk(done));
        }
        // Record the manifest so the folder is self-describing (and survives offline use).
        let mp = dir.join("manifest.json");
        let body = serde_json::to_vec_pretty(&m)?;
        let tmp = dir.join("manifest.json.part");
        tokio::fs::write(&tmp, body).await?;
        tokio::fs::rename(&tmp, &mp).await?;
        Ok(dir)
    }

    async fn download(&self, f: &ManifestFile, dest: &Path, mut on_bytes: impl FnMut(u64)) -> Result<()> {
        if let Some(parent) = dest.parent() { tokio::fs::create_dir_all(parent).await?; }
        let part = dest.with_extension(part_ext(dest));
        let _ = tokio::fs::remove_file(dest).await; // mismatched/corrupt final file

        for attempt in 0..2 {
            if let Some(local) = f.url.strip_prefix("file://") {
                // Test/offline-mirror hook: copy from a local path, honoring resume offset.
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut src = tokio::fs::File::open(local).await.map_err(|e| anyhow!("cannot read {local}: {e}"))?;
                let mut have = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
                if have > f.size || attempt > 0 { let _ = tokio::fs::remove_file(&part).await; have = 0; }
                src.seek(std::io::SeekFrom::Start(have)).await?;
                let mut out = tokio::fs::OpenOptions::new().create(true).append(true).open(&part).await?;
                let mut buf = vec![0u8; 1 << 16];
                let mut got = have;
                on_bytes(got);
                loop {
                    let n = src.read(&mut buf).await?;
                    if n == 0 { break; }
                    out.write_all(&buf[..n]).await?;
                    got += n as u64;
                    on_bytes(got.min(f.size));
                }
                out.flush().await?;
                drop(out);
            } else {
            let mut have = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
            if have > f.size { let _ = tokio::fs::remove_file(&part).await; have = 0; }
            if have < f.size || attempt > 0 {
                let mut req = self.client.get(&f.url);
                if have > 0 { req = req.header(reqwest::header::RANGE, format!("bytes={have}-")); }
                let resp = req.send().await.map_err(|e| anyhow!(
                    "cannot reach {} ({e}); check your internet connection, or copy the model folder into {}", f.url, dest.parent().map(|p| p.display().to_string()).unwrap_or_default()))?;
                let status = resp.status();
                let mut file = if have > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT {
                    tokio::fs::OpenOptions::new().append(true).open(&part).await?
                } else if status.is_success() {
                    have = 0; // server ignored Range (200): restart
                    tokio::fs::File::create(&part).await?
                } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                    let _ = tokio::fs::remove_file(&part).await;
                    continue;
                } else {
                    bail!("cannot download {} (HTTP {status}); check your internet connection or copy the model folder in manually", f.url);
                };
                let mut got = have;
                on_bytes(got);
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| anyhow!("download interrupted ({e}); rerun to resume"))?;
                    file.write_all(&chunk).await?;
                    got += chunk.len() as u64;
                    on_bytes(got.min(f.size));
                }
                file.flush().await?;
                drop(file);
            }
            }
            let p2 = part.clone();
            let h = tokio::task::spawn_blocking(move || sha256_file(&p2)).await??;
            let len = tokio::fs::metadata(&part).await?.len();
            if len == f.size && h.eq_ignore_ascii_case(&f.sha256) {
                tokio::fs::rename(&part, dest).await?;
                return Ok(());
            }
            // Corrupt (bad resume splice or tampered): discard and retry once from scratch.
            let _ = tokio::fs::remove_file(&part).await;
            if attempt == 1 {
                bail!("sha256/size mismatch for {} (got {len} bytes, sha256 {h})", f.path);
            }
        }
        bail!("download failed for {}", f.path)
    }
}

fn file_matches(p: &Path, f: &ManifestFile) -> bool {
    match std::fs::metadata(p) {
        Ok(md) if md.is_file() && md.len() == f.size => sha256_file(p).map(|h| h.eq_ignore_ascii_case(&f.sha256)).unwrap_or(false),
        _ => false,
    }
}

fn part_ext(p: &Path) -> String {
    match p.extension() { Some(e) => format!("{}.part", e.to_string_lossy()), None => "part".into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `files` under a scratch dir; URLs are `file://` (see download hook).
    fn serve(files: Vec<(String, Vec<u8>)>) -> String {
        let d = tempfile::tempdir().unwrap().keep();
        for (p, data) in files {
            let f = d.join(p.trim_start_matches('/'));
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, data).unwrap();
        }
        format!("file://{}", d.display())
    }

    fn manifest_for(base: &str, files: &[(&str, &[u8])]) -> Manifest {
        Manifest {
            name: "toy".into(),
            version: "1".into(),
            files: files.iter().map(|(p, d)| ManifestFile {
                path: p.to_string(),
                url: format!("{base}/{p}"),
                sha256: hex::encode(Sha256::digest(d)),
                size: d.len() as u64,
            }).collect(),
        }
    }

    fn setup(m: &Manifest) -> (tempfile::TempDir, ModelManager) {
        let t = tempfile::tempdir().unwrap();
        let home = DaouchiHome::at(t.path());
        let dir = home.model_dir(&m.name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), serde_json::to_vec(m).unwrap()).unwrap();
        (t, ModelManager::new(home))
    }

    #[test]
    fn builtin_manifests_parse() {
        for n in ["clap", "ast"] {
            let m = builtin_manifest(n).unwrap();
            assert_eq!(m.name, n);
            assert!(!m.files.is_empty());
            for f in &m.files {
                assert!(is_safe_rel(&f.path));
                assert_eq!(f.sha256.len(), 64, "{}", f.path);
                assert!(f.url.starts_with("https://huggingface.co/"));
            }
        }
        assert!(builtin_manifest("nope").is_none());
    }

    #[test]
    fn preseeded_is_ready_without_network() {
        let a = b"hello onnx".to_vec();
        let m = manifest_for("http://127.0.0.1:1", &[("onnx/a.bin", &a)]);
        let (_t, mm) = setup(&m);
        assert_eq!(mm.status("toy").unwrap(), ModelStatus::Missing);
        let p = mm.file_path("toy", "onnx/a.bin");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &a).unwrap();
        assert_eq!(mm.status("toy").unwrap(), ModelStatus::Ready);
        std::fs::write(&p, b"corrupt!!!").unwrap();
        assert_eq!(mm.status("toy").unwrap(), ModelStatus::Partial);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn downloads_verifies_and_resumes() {
        let a: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let b = b"tiny".to_vec();
        let base = serve(vec![("/onnx/a.bin".into(), a.clone()), ("/b.txt".into(), b.clone())]);
        let m = manifest_for(&base, &[("onnx/a.bin", &a), ("b.txt", &b)]);
        let (_t, mm) = setup(&m);
        // simulate an interrupted download: half of a.bin as .part
        let dest = mm.file_path("toy", "onnx/a.bin");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest.with_extension("bin.part"), &a[..100_000]).unwrap();
        assert_eq!(mm.status("toy").unwrap(), ModelStatus::Partial);
        let mut last = None;
        mm.ensure("toy", |p| last = Some(p)).await.unwrap();
        let last = last.unwrap();
        assert_eq!(last.bytes_done, last.bytes_total);
        assert_eq!(std::fs::read(&dest).unwrap(), a);
        assert!(!dest.with_extension("bin.part").exists());
        assert_eq!(mm.status("toy").unwrap(), ModelStatus::Ready);
        // idempotent, no network needed when ready
        mm.ensure("toy", |_| {}).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_checksum_rejected_and_offline_error_is_clear() {
        let a = b"real content".to_vec();
        let base = serve(vec![("/a.bin".into(), b"evil bytes!!".to_vec())]);
        let m = manifest_for(&base, &[("a.bin", &a)]);
        let (_t, mm) = setup(&m);
        let e = mm.ensure("toy", |_| {}).await.unwrap_err();
        assert!(format!("{e:#}").contains("mismatch"), "{e:#}");
        assert!(!mm.file_path("toy", "a.bin").exists());

        let m2 = manifest_for("http://127.0.0.1:1", &[("a.bin", &a)]);
        let (_t2, mm2) = setup(&m2);
        let e = mm2.ensure("toy", |_| {}).await.unwrap_err();
        assert!(format!("{e:#}").contains("cannot reach") || format!("{e:#}").contains("cannot download"), "{e:#}");
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(!is_safe_rel("../x"));
        assert!(!is_safe_rel("/etc/passwd"));
        assert!(is_safe_rel("onnx/model.onnx"));
    }
}
