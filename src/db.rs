use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

pub fn db_path() -> Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("no data dir"))?;
    let dir = base.join("webtorapp");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("downloads.db"))
}

pub fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS downloads (
            id          TEXT PRIMARY KEY,
            url         TEXT NOT NULL,
            filename    TEXT NOT NULL,
            dest_path   TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'queued',
            priority    INTEGER NOT NULL DEFAULT 0,
            threads     INTEGER NOT NULL DEFAULT 4,
            scheduled_at TEXT,
            total_bytes  INTEGER,
            bytes_done   INTEGER NOT NULL DEFAULT 0,
            error_msg   TEXT,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS torrents (
            info_hash    TEXT PRIMARY KEY,
            title        TEXT NOT NULL,
            source_label TEXT NOT NULL,
            output_dir   TEXT NOT NULL,
            routed_files TEXT NOT NULL DEFAULT '[]',
            created_at   TEXT NOT NULL
        );",
    )?;
    Ok(())
}

pub fn open() -> Result<Connection> {
    let path = db_path()?;
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    init_db(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_downloads_table_with_expected_columns() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(downloads);").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for expected in [
            "id", "url", "filename", "dest_path", "status", "priority", "threads",
            "scheduled_at", "total_bytes", "bytes_done", "error_msg", "created_at", "updated_at",
        ] {
            assert!(columns.contains(&expected.to_string()), "missing column {expected}");
        }
    }

    #[test]
    fn creates_torrents_table_with_expected_columns() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(torrents);").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        for expected in ["info_hash", "title", "source_label", "output_dir", "routed_files", "created_at"] {
            assert!(columns.contains(&expected.to_string()), "missing column {expected}");
        }
    }
}
