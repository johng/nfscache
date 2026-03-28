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

pub struct DirEntry {
    pub dir_path: String,
    pub total_size: u64,
    pub last_accessed: f64,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
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
            );
            CREATE TABLE IF NOT EXISTS directories (
                dir_path TEXT PRIMARY KEY,
                total_size INTEGER NOT NULL DEFAULT 0,
                last_accessed REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pending_writes (
                path TEXT PRIMARY KEY,
                created_at REAL NOT NULL
            );"
        )?;
        Ok(Self { conn })
    }

    pub fn add(&self, path: &str, size: u64, mtime: f64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO cache (path, size, mtime, cached_at) VALUES (?1, ?2, ?3, ?4)",
            params![path, size as i64, mtime, now_secs()],
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

    pub fn all_cached_paths(&self) -> rusqlite::Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM cache")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = std::collections::HashSet::new();
        for row in rows {
            set.insert(row?);
        }
        Ok(set)
    }

    // --- Directory-level tracking ---

    /// Update a directory's last_accessed time without changing its size.
    pub fn touch_dir_access(&self, dir_path: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE directories SET last_accessed = ?1 WHERE dir_path = ?2",
            params![now_secs(), dir_path],
        )?;
        Ok(())
    }

    /// Record or update a directory's access time and total size.
    pub fn touch_dir(&self, dir_path: &str, total_size: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO directories (dir_path, total_size, last_accessed) VALUES (?1, ?2, ?3)
             ON CONFLICT(dir_path) DO UPDATE SET last_accessed = ?3, total_size = ?2",
            params![dir_path, total_size as i64, now_secs()],
        )?;
        Ok(())
    }

    /// Get all tracked directories sorted by least recently accessed.
    pub fn lru_directories(&self) -> rusqlite::Result<Vec<DirEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT dir_path, total_size, last_accessed FROM directories ORDER BY last_accessed ASC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DirEntry {
                dir_path: row.get(0)?,
                total_size: row.get::<_, i64>(1)? as u64,
                last_accessed: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Remove all cache entries for files in a directory and the directory record itself.
    pub fn remove_dir(&self, dir_path: &str) -> rusqlite::Result<()> {
        // Remove files: "dir_path/%" matches all files in the directory
        let pattern = format!("{}/%", dir_path);
        self.conn.execute(
            "DELETE FROM cache WHERE path LIKE ?1",
            params![pattern],
        )?;
        self.conn.execute(
            "DELETE FROM directories WHERE dir_path = ?1",
            params![dir_path],
        )?;
        Ok(())
    }

    // --- Pending writes (files written locally, not yet synced to NAS) ---

    /// Mark a file as needing sync to NAS.
    pub fn add_pending_write(&self, path: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO pending_writes (path, created_at) VALUES (?1, ?2)",
            params![path, now_secs()],
        )?;
        Ok(())
    }

    /// Remove a file from pending writes (after successful NAS sync).
    pub fn remove_pending_write(&self, path: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM pending_writes WHERE path = ?1",
            params![path],
        )?;
        Ok(())
    }

    /// Get all files pending NAS sync.
    pub fn all_pending_writes(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT path FROM pending_writes ORDER BY created_at ASC")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut paths = Vec::new();
        for row in rows {
            paths.push(row?);
        }
        Ok(paths)
    }

    /// Check if a directory is tracked (i.e., has been cached).
    pub fn is_dir_cached(&self, dir_path: &str) -> rusqlite::Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM directories WHERE dir_path = ?1",
            params![dir_path],
            |row| row.get(0),
        )?;
        Ok(count > 0)
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

    #[test]
    fn test_touch_and_lru_directories() {
        let db = test_db();
        db.touch_dir("old_dir", 1000).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        db.touch_dir("new_dir", 2000).unwrap();
        let dirs = db.lru_directories().unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].dir_path, "old_dir");
        assert_eq!(dirs[0].total_size, 1000);
        assert_eq!(dirs[1].dir_path, "new_dir");
    }

    #[test]
    fn test_remove_dir() {
        let db = test_db();
        db.add("March 2026/a.jpg", 100, 1.0).unwrap();
        db.add("March 2026/b.jpg", 200, 2.0).unwrap();
        db.add("April 2026/c.jpg", 300, 3.0).unwrap();
        db.touch_dir("March 2026", 300).unwrap();
        db.touch_dir("April 2026", 300).unwrap();

        db.remove_dir("March 2026").unwrap();

        assert!(db.get("March 2026/a.jpg").unwrap().is_none());
        assert!(db.get("March 2026/b.jpg").unwrap().is_none());
        assert!(db.get("April 2026/c.jpg").unwrap().is_some());
        assert!(!db.is_dir_cached("March 2026").unwrap());
        assert!(db.is_dir_cached("April 2026").unwrap());
    }

    #[test]
    fn test_is_dir_cached() {
        let db = test_db();
        assert!(!db.is_dir_cached("nope").unwrap());
        db.touch_dir("yep", 100).unwrap();
        assert!(db.is_dir_cached("yep").unwrap());
    }

    #[test]
    fn test_touch_dir_access_updates_time_not_size() {
        let db = test_db();
        db.touch_dir("photos", 5000).unwrap();
        let dirs_before = db.lru_directories().unwrap();
        let original_size = dirs_before[0].total_size;
        let original_accessed = dirs_before[0].last_accessed;

        std::thread::sleep(std::time::Duration::from_millis(20));
        db.touch_dir_access("photos").unwrap();

        let dirs_after = db.lru_directories().unwrap();
        assert_eq!(dirs_after[0].total_size, original_size, "total_size should not change");
        assert!(dirs_after[0].last_accessed > original_accessed, "last_accessed should increase");
    }

    #[test]
    fn test_touch_dir_access_nonexistent_is_noop() {
        let db = test_db();
        // Should not error on a dir that doesn't exist
        db.touch_dir_access("nonexistent").unwrap();
        assert!(db.lru_directories().unwrap().is_empty());
    }

    #[test]
    fn test_pending_write_lifecycle() {
        let db = test_db();

        // Initially empty
        assert!(db.all_pending_writes().unwrap().is_empty());

        // Add pending writes
        db.add_pending_write("March 2026/IMG_001.jpg").unwrap();
        db.add_pending_write("March 2026/IMG_002.jpg").unwrap();

        let pending = db.all_pending_writes().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.contains(&"March 2026/IMG_001.jpg".to_string()));
        assert!(pending.contains(&"March 2026/IMG_002.jpg".to_string()));

        // Remove one
        db.remove_pending_write("March 2026/IMG_001.jpg").unwrap();
        let pending = db.all_pending_writes().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0], "March 2026/IMG_002.jpg");

        // Remove the other
        db.remove_pending_write("March 2026/IMG_002.jpg").unwrap();
        assert!(db.all_pending_writes().unwrap().is_empty());
    }

    #[test]
    fn test_add_pending_write_is_idempotent() {
        let db = test_db();
        db.add_pending_write("file.jpg").unwrap();
        db.add_pending_write("file.jpg").unwrap();
        assert_eq!(db.all_pending_writes().unwrap().len(), 1);
    }

    #[test]
    fn test_remove_pending_write_nonexistent_is_noop() {
        let db = test_db();
        db.remove_pending_write("nonexistent.jpg").unwrap();
    }

    #[test]
    fn test_pending_writes_ordered_by_created_at() {
        let db = test_db();
        db.add_pending_write("first.jpg").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        db.add_pending_write("second.jpg").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        db.add_pending_write("third.jpg").unwrap();

        let pending = db.all_pending_writes().unwrap();
        assert_eq!(pending, vec!["first.jpg", "second.jpg", "third.jpg"]);
    }

    #[test]
    fn test_remove_dir_does_not_clean_pending_writes() {
        let db = test_db();
        db.add("March 2026/a.jpg", 100, 1.0).unwrap();
        db.touch_dir("March 2026", 100).unwrap();
        db.add_pending_write("March 2026/a.jpg").unwrap();

        db.remove_dir("March 2026").unwrap();

        // Cache entry and dir record removed
        assert!(db.get("March 2026/a.jpg").unwrap().is_none());
        assert!(!db.is_dir_cached("March 2026").unwrap());
        // But pending write survives — it's in a separate table
        let pending = db.all_pending_writes().unwrap();
        assert_eq!(pending, vec!["March 2026/a.jpg"]);
    }
}
