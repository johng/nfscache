// src/cache_db.rs
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct CacheEntry {
    pub path: String,
    pub size: u64,
    pub mtime: f64,
    pub cached_at: f64,
}

pub struct CacheDB {
    conn: Connection,
}

impl CacheDB {
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cache (
                path TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                mtime REAL NOT NULL,
                cached_at REAL NOT NULL
            )"
        )?;
        Ok(Self { conn })
    }

    pub fn add(&self, path: &str, size: u64, mtime: f64) -> rusqlite::Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        self.conn.execute(
            "INSERT OR REPLACE INTO cache (path, size, mtime, cached_at) VALUES (?1, ?2, ?3, ?4)",
            params![path, size as i64, mtime, now],
        )?;
        Ok(())
    }

    pub fn get(&self, path: &str) -> rusqlite::Result<Option<CacheEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size, mtime, cached_at FROM cache WHERE path = ?1"
        )?;
        let mut rows = stmt.query_map(params![path], |row| {
            Ok(CacheEntry {
                path: row.get(0)?,
                size: row.get::<_, i64>(1)? as u64,
                mtime: row.get(2)?,
                cached_at: row.get(3)?,
            })
        })?;
        match rows.next() {
            Some(Ok(entry)) => Ok(Some(entry)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    pub fn remove(&self, path: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM cache WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn total_size(&self) -> rusqlite::Result<u64> {
        let size: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM cache", [], |row| row.get(0)
        )?;
        Ok(size as u64)
    }

    pub fn oldest_entries(&self, limit: usize) -> rusqlite::Result<Vec<CacheEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size, mtime, cached_at FROM cache ORDER BY mtime ASC LIMIT ?1"
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(CacheEntry {
                path: row.get(0)?,
                size: row.get::<_, i64>(1)? as u64,
                mtime: row.get(2)?,
                cached_at: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn all_cached_paths(&self) -> rusqlite::Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM cache")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = std::collections::HashSet::new();
        for row in rows {
            set.insert(row?);
        }
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_db() -> CacheDB {
        CacheDB::open(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn test_add_and_get() {
        let db = test_db();
        db.add("March 2026/IMG_001.jpg", 5_000_000, 1_700_000_000.0).unwrap();
        let entry = db.get("March 2026/IMG_001.jpg").unwrap().unwrap();
        assert_eq!(entry.size, 5_000_000);
        assert_eq!(entry.mtime, 1_700_000_000.0);
    }

    #[test]
    fn test_get_nonexistent() {
        let db = test_db();
        assert!(db.get("nope.jpg").unwrap().is_none());
    }

    #[test]
    fn test_remove() {
        let db = test_db();
        db.add("test.jpg", 100, 1_700_000_000.0).unwrap();
        db.remove("test.jpg").unwrap();
        assert!(db.get("test.jpg").unwrap().is_none());
    }

    #[test]
    fn test_total_size() {
        let db = test_db();
        db.add("a.jpg", 1000, 1_700_000_000.0).unwrap();
        db.add("b.jpg", 2000, 1_700_000_001.0).unwrap();
        assert_eq!(db.total_size().unwrap(), 3000);
    }

    #[test]
    fn test_oldest_entries() {
        let db = test_db();
        db.add("old.jpg", 100, 1_600_000_000.0).unwrap();
        db.add("new.jpg", 200, 1_800_000_000.0).unwrap();
        db.add("mid.jpg", 150, 1_700_000_000.0).unwrap();
        let oldest = db.oldest_entries(2).unwrap();
        let paths: Vec<&str> = oldest.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["old.jpg", "mid.jpg"]);
    }

    #[test]
    fn test_all_cached_paths() {
        let db = test_db();
        db.add("a.jpg", 100, 1_700_000_000.0).unwrap();
        db.add("b.jpg", 200, 1_700_000_001.0).unwrap();
        let paths = db.all_cached_paths().unwrap();
        assert!(paths.contains("a.jpg"));
        assert!(paths.contains("b.jpg"));
    }

    #[test]
    fn test_update_existing() {
        let db = test_db();
        db.add("a.jpg", 100, 1_700_000_000.0).unwrap();
        db.add("a.jpg", 200, 1_700_000_001.0).unwrap();
        let entry = db.get("a.jpg").unwrap().unwrap();
        assert_eq!(entry.size, 200);
        assert_eq!(entry.mtime, 1_700_000_001.0);
    }
}
