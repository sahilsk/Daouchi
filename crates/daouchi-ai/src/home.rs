//! `~/.daouchi` layout. Root is `$DAOUCHI_HOME` or `<home>/.daouchi`.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaouchiHome {
    root: PathBuf,
}

impl DaouchiHome {
    /// Resolve from `DAOUCHI_HOME` (if non-empty) else `dirs::home_dir()/.daouchi`.
    pub fn resolve() -> anyhow::Result<Self> {
        match std::env::var_os("DAOUCHI_HOME") {
            Some(v) if !v.is_empty() => Ok(Self::at(PathBuf::from(v))),
            _ => {
                let h = dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("cannot determine home directory; set DAOUCHI_HOME"))?;
                Ok(Self::at(h.join(".daouchi")))
            }
        }
    }

    /// Explicit root (tests, embedding).
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path { &self.root }
    pub fn data_dir(&self) -> PathBuf { self.root.join("data") }
    pub fn models_dir(&self) -> PathBuf { self.root.join("models") }
    pub fn cache_dir(&self) -> PathBuf { self.root.join("cache") }
    pub fn exports_dir(&self) -> PathBuf { self.root.join("exports") }
    pub fn coreml_cache_dir(&self) -> PathBuf { self.cache_dir().join("coreml") }
    /// Where a user-installed ONNX Runtime dylib may live.
    pub fn lib_dir(&self) -> PathBuf { self.root.join("lib") }
    pub fn model_dir(&self, name: &str) -> PathBuf { self.models_dir().join(name) }

    /// Create all directories (idempotent).
    pub fn ensure(&self) -> anyhow::Result<()> {
        for d in [self.data_dir(), self.models_dir(), self.cache_dir(), self.exports_dir(), self.coreml_cache_dir()] {
            std::fs::create_dir_all(&d).map_err(|e| anyhow::anyhow!("create {}: {e}", d.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_ensure() {
        let t = tempfile::tempdir().unwrap();
        let h = DaouchiHome::at(t.path());
        assert_eq!(h.model_dir("clap"), t.path().join("models/clap"));
        assert_eq!(h.coreml_cache_dir(), t.path().join("cache/coreml"));
        h.ensure().unwrap();
        h.ensure().unwrap();
        for d in [h.data_dir(), h.models_dir(), h.cache_dir(), h.exports_dir(), h.coreml_cache_dir()] {
            assert!(d.is_dir());
        }
    }

    #[test]
    fn env_override() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t = tempfile::tempdir().unwrap();
        let old = std::env::var_os("DAOUCHI_HOME");
        std::env::set_var("DAOUCHI_HOME", t.path());
        assert_eq!(DaouchiHome::resolve().unwrap().root(), t.path());
        std::env::set_var("DAOUCHI_HOME", "");
        assert!(DaouchiHome::resolve().unwrap().root().ends_with(".daouchi"));
        match old { Some(v) => std::env::set_var("DAOUCHI_HOME", v), None => std::env::remove_var("DAOUCHI_HOME") }
    }
}
