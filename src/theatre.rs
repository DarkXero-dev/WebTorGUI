use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

/// A title saved from Discover's heart icon to watch later - just enough of
/// its Stremio catalog entry to redraw the same poster card and re-open the
/// same source picker (`kind`/`id`/`name`), no download/session data.
pub struct TheatreEntry {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub poster: String,
    pub year: String,
    pub imdb_rating: String,
}

pub fn add(conn: &Connection, entry: &TheatreEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO theatre (id, kind, name, poster, year, imdb_rating, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO NOTHING",
        params![entry.id, entry.kind, entry.name, entry.poster, entry.year, entry.imdb_rating, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM theatre WHERE id = ?1", params![id])?;
    Ok(())
}

/// Newest-added first, so the in-memory list a caller builds from this
/// never needs re-sorting after later inserts.
pub fn list_all(conn: &Connection) -> Result<Vec<TheatreEntry>> {
    let mut stmt = conn.prepare("SELECT id, kind, name, poster, year, imdb_rating FROM theatre ORDER BY added_at DESC")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(TheatreEntry {
                id: row.get(0)?,
                kind: row.get(1)?,
                name: row.get(2)?,
                poster: row.get(3)?,
                year: row.get(4)?,
                imdb_rating: row.get(5)?,
            })
        })?
        .filter_map(std::result::Result::ok)
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> TheatreEntry {
        TheatreEntry {
            id: id.to_string(),
            kind: "movie".to_string(),
            name: "My Movie".to_string(),
            poster: "https://example.com/poster.jpg".to_string(),
            year: "2024".to_string(),
            imdb_rating: "7.5".to_string(),
        }
    }

    #[test]
    fn add_then_list_round_trips() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        add(&conn, &entry("tt123")).unwrap();

        let all = list_all(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "tt123");
        assert_eq!(all[0].name, "My Movie");
    }

    #[test]
    fn adding_the_same_id_twice_does_not_duplicate() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();

        add(&conn, &entry("tt123")).unwrap();
        add(&conn, &entry("tt123")).unwrap();

        assert_eq!(list_all(&conn).unwrap().len(), 1);
    }

    #[test]
    fn remove_deletes_the_row() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        add(&conn, &entry("tt123")).unwrap();

        remove(&conn, "tt123").unwrap();

        assert!(list_all(&conn).unwrap().is_empty());
    }
}
