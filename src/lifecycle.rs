//! Storage layout. Everything lives under `DaouchiHome` (`~/.daouchi`, override `DAOUCHI_HOME`).
use daouchi_ai::home::DaouchiHome;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static HOME: OnceLock<DaouchiHome> = OnceLock::new();

/// Resolve the home directory once and create its folders.
pub fn init() -> anyhow::Result<&'static DaouchiHome> {
    if HOME.get().is_none() {
        let h = DaouchiHome::resolve()?;
        h.ensure()?;
        let _ = HOME.set(h);
    }
    Ok(HOME.get().expect("home initialised"))
}

pub fn home() -> &'static DaouchiHome {
    init().expect("cannot resolve DAOUCHI_HOME / home directory")
}

pub fn db_path() -> PathBuf {
    home().data_dir().join("sounds.db")
}

pub fn cache_dir() -> PathBuf {
    home().cache_dir()
}

pub fn stems_cache_dir() -> PathBuf {
    cache_dir().join("stems")
}

/// Candidate locations of the pre-standalone project database (`<project root>/.daouchi/data/sounds.db`).
fn legacy_db_candidates() -> Vec<PathBuf> {
    let mut c = Vec::new();
    // exe at <root>/target/{debug,release}/daouchi
    if let Some(root) = std::env::current_exe().ok().and_then(|e| e.parent()?.parent()?.parent().map(PathBuf::from)) {
        c.push(root.join(".daouchi/data/sounds.db"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        c.push(cwd.join(".daouchi/data/sounds.db"));
    }
    // dev builds: source tree the binary was compiled from
    c.push(Path::new(env!("CARGO_MANIFEST_DIR")).join(".daouchi/data/sounds.db"));
    c
}

/// One-time migration: if `dest` does not exist and a legacy project database does, COPY it
/// (never move/delete the source). Embeddings in it are tagged as legacy by the schema migration.
pub fn migrate_legacy_db(dest: &Path) {
    if dest.exists() {
        return;
    }
    let Some(src) = legacy_db_candidates().into_iter().find(|p| p.is_file()) else { return };
    let Some(dir) = dest.parent() else { return };
    let tmp = dir.join("sounds.db.migrating");
    let res = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::copy(&src, &tmp))
        .and_then(|_| std::fs::rename(&tmp, dest));
    match res {
        Ok(()) => tracing::info!("migrated legacy database: copied {} -> {}", src.display(), dest.display()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!("legacy database migration from {} failed: {e}", src.display());
        }
    }
}
