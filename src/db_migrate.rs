use rusqlite::{Connection, Result};

/// Bumped when a migration step is added. All steps are additive and idempotent (feature
/// detected), so databases with any older `user_version` upgrade safely; nothing is dropped.
pub const CURRENT_VERSION: u32 = 7;

fn has_column(conn: &Connection, table: &str, col: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let found = stmt.query_map([], |r| r.get::<_, String>(1))?.flatten().any(|n| n == col);
    Ok(found)
}

pub fn init_and_migrate(conn: &Connection) -> Result<()> {
    let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // schema.sql is all CREATE ... IF NOT EXISTS, so running it is harmless on any version.
    conn.execute_batch(include_str!("../schema.sql"))?;

    // embeddings.model: which embedding model produced the vector. Rows written before this
    // column existed came from the legacy LAION checkpoint and are ignored by every query.
    if !has_column(conn, "embeddings", "model")? {
        conn.execute_batch(
            "ALTER TABLE embeddings ADD COLUMN model TEXT NOT NULL DEFAULT 'laion-630k-legacy';",
        )?;
        // Coordinates computed from legacy-model vectors are meaningless in the new space.
        conn.execute_batch("DELETE FROM radar_coords;")?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_embeddings_model ON embeddings(model);")?;

    if version < CURRENT_VERSION {
        conn.execute_batch(&format!("PRAGMA user_version = {CURRENT_VERSION}"))?;
    }
    Ok(())
}
