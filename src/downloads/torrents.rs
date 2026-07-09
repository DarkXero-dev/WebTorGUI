use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

/// A torrent's own display metadata (title, source, output folder, which
/// files have already been auto-routed) - librqbit's own session
/// persistence resumes the actual download, but knows nothing about any of
/// this, so without this table the Downloads page would come back empty
/// (and look like history was wiped) on every restart.
pub struct StoredTorrent {
    pub title: String,
    pub source_label: String,
    pub output_dir: String,
    pub routed_files: Vec<usize>,
}

pub fn upsert(
    conn: &Connection,
    info_hash: &str,
    title: &str,
    source_label: &str,
    output_dir: &str,
    routed_files: &std::collections::HashSet<usize>,
) -> Result<()> {
    let routed_json = serde_json::to_string(&routed_files.iter().collect::<Vec<_>>())?;
    conn.execute(
        "INSERT INTO torrents (info_hash, title, source_label, output_dir, routed_files, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(info_hash) DO UPDATE SET routed_files = excluded.routed_files",
        params![info_hash, title, source_label, output_dir, routed_json, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, info_hash: &str) -> Option<StoredTorrent> {
    conn.query_row(
        "SELECT title, source_label, output_dir, routed_files FROM torrents WHERE info_hash = ?1",
        params![info_hash],
        |row| {
            let routed_json: String = row.get(3)?;
            let routed_files: Vec<usize> = serde_json::from_str(&routed_json).unwrap_or_default();
            Ok(StoredTorrent {
                title: row.get(0)?,
                source_label: row.get(1)?,
                output_dir: row.get(2)?,
                routed_files,
            })
        },
    )
    .ok()
}

pub fn remove(conn: &Connection, info_hash: &str) -> Result<()> {
    conn.execute("DELETE FROM torrents WHERE info_hash = ?1", params![info_hash])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_get_round_trips() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        let mut routed = std::collections::HashSet::new();
        routed.insert(2usize);
        upsert(&conn, "abc123", "My Movie", "Magnet link", "/downloads/abc", &routed).unwrap();

        let stored = get(&conn, "abc123").unwrap();
        assert_eq!(stored.title, "My Movie");
        assert_eq!(stored.source_label, "Magnet link");
        assert_eq!(stored.output_dir, "/downloads/abc");
        assert_eq!(stored.routed_files, vec![2]);
    }

    #[test]
    fn remove_deletes_the_row() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        upsert(&conn, "abc123", "Title", "Magnet link", "/dl", &std::collections::HashSet::new()).unwrap();

        remove(&conn, "abc123").unwrap();

        assert!(get(&conn, "abc123").is_none());
    }
}
