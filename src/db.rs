use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};
use anyhow::Result;

/// Thread-safe SQLite connection pool (single connection behind a Mutex).
#[derive(Clone)]
pub struct DbPool(pub Arc<Mutex<Connection>>);

impl DbPool {
    pub fn get(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().expect("db mutex poisoned")
    }
}

/// Open the SQLite database at `path` and return the pool.
///
/// Uses DELETE journal mode + 5-second busy timeout — compatible with exFAT
/// and other filesystems that don't support mmap-based WAL shared memory.
///
/// If the database file was previously created in WAL mode (e.g. on a
/// different filesystem), `Connection::open` itself will fail with
/// SQLITE_IOERR_SHMOPEN before we can run any PRAGMAs. In that case we wipe
/// the WAL artefacts, set the database file aside (`.unreadable.bak`) and start fresh — the user will
/// need to re-index, but the app will be operational.
pub fn open(path: &Path) -> Result<DbPool> {
    // Remove any leftover WAL / SHM files so SQLite doesn't try to recover
    // a WAL-mode database on open (which calls xShmMap and fails on exFAT).
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let _ = std::fs::remove_file(path.with_file_name(format!("{name}-wal")));
    let _ = std::fs::remove_file(path.with_file_name(format!("{name}-shm")));

    let conn = match Connection::open(path) {
        Ok(c) => c,
        Err(rusqlite::Error::SqliteFailure(err, _)) if err.extended_code == 4618 || err.extended_code & 0xFF == 10 => {
            // The database file has a WAL-mode header that requires shared
            // memory — not available on exFAT. Wipe it and start fresh.
            eprintln!(
                "[db] WAL shared-memory unavailable on this filesystem \
                 (exFAT/FAT32); recreating database — please re-index your library."
            );
            // Keep the old file as a backup instead of deleting user data.
            let _ = std::fs::rename(path, path.with_file_name(format!("{name}.unreadable.bak")));
            Connection::open(path)?
        }
        Err(e) => return Err(e.into()),
    };

    conn.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
    )?;

    Ok(DbPool(Arc::new(Mutex::new(conn))))
}
