// src/sync.rs
use crate::cache_db::CacheDB;
use log::{debug, info};
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

const PHOTO_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "dng", "raw", "tiff", "tif", "cr2", "nef", "arw",
];

pub struct FileInfo {
    pub rel_path: String,
    pub size: u64,
    pub mtime: f64,
}

pub struct SyncEngine {
    nas_path: PathBuf,
    cache_dir: PathBuf,
    db: CacheDB,
    max_cache_bytes: u64,
}

impl SyncEngine {
    pub fn new(nas_path: PathBuf, cache_dir: PathBuf, db: CacheDB, max_cache_bytes: u64) -> Self {
        Self { nas_path, cache_dir, db, max_cache_bytes }
    }

    pub fn scan_nas(&self) -> Vec<FileInfo> {
        let mut files = Vec::new();
        for entry in WalkDir::new(&self.nas_path).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if !PHOTO_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            if let Ok(meta) = fs::metadata(path) {
                let rel_path = path.strip_prefix(&self.nas_path)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let mtime = meta.modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64();
                files.push(FileInfo {
                    rel_path,
                    size: meta.len(),
                    mtime,
                });
            }
        }
        files
    }

    pub fn sync(&self) {
        info!("Starting sync cycle");
        let mut files = self.scan_nas();
        files.sort_by(|a, b| b.mtime.partial_cmp(&a.mtime).unwrap());

        let cached_paths = self.db.all_cached_paths().unwrap_or_default();
        let mut current_size = self.db.total_size().unwrap_or(0);

        for f in &files {
            if cached_paths.contains(&f.rel_path) {
                if let Ok(Some(entry)) = self.db.get(&f.rel_path) {
                    if entry.mtime == f.mtime {
                        continue;
                    }
                }
            }
            // Evict oldest entries to make room if needed
            while current_size + f.size > self.max_cache_bytes {
                let oldest = match self.db.oldest_entries(1) {
                    Ok(entries) if !entries.is_empty() => entries,
                    _ => break,
                };
                let entry = &oldest[0];
                // Don't evict if oldest entry is newer than or equal to what we're trying to cache
                if entry.mtime >= f.mtime {
                    break;
                }
                let evicted_size = entry.size;
                let evicted_path = entry.path.clone();
                let local_path = self.cache_dir.join(&evicted_path);
                if local_path.exists() {
                    let _ = fs::remove_file(&local_path);
                    if let Some(parent) = local_path.parent() {
                        if parent != self.cache_dir && fs::read_dir(parent).map(|mut d| d.next().is_none()).unwrap_or(false) {
                            let _ = fs::remove_dir(parent);
                        }
                    }
                }
                let _ = self.db.remove(&evicted_path);
                current_size -= evicted_size;
                debug!("Evicted: {}", evicted_path);
            }
            if current_size + f.size > self.max_cache_bytes {
                continue;
            }
            if self.cache_file(f).is_ok() {
                current_size += f.size;
            }
        }

        self.evict_until_under_limit();
        info!("Sync cycle complete. Cache size: {} bytes", self.db.total_size().unwrap_or(0));
    }

    fn cache_file(&self, file_info: &FileInfo) -> std::io::Result<()> {
        let src = self.nas_path.join(&file_info.rel_path);
        let dst = self.cache_dir.join(&file_info.rel_path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst)?;
        self.db.add(&file_info.rel_path, file_info.size, file_info.mtime).ok();
        debug!("Cached: {}", file_info.rel_path);
        Ok(())
    }

    fn evict_until_under_limit(&self) {
        while self.db.total_size().unwrap_or(0) > self.max_cache_bytes {
            let oldest = match self.db.oldest_entries(1) {
                Ok(entries) if !entries.is_empty() => entries,
                _ => break,
            };
            let entry = &oldest[0];
            let local_path = self.cache_dir.join(&entry.path);
            if local_path.exists() {
                let _ = fs::remove_file(&local_path);
                if let Some(parent) = local_path.parent() {
                    if parent != self.cache_dir && fs::read_dir(parent).map(|mut d| d.next().is_none()).unwrap_or(false) {
                        let _ = fs::remove_dir(parent);
                    }
                }
            }
            let _ = self.db.remove(&entry.path);
            debug!("Evicted: {}", entry.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (SyncEngine, TempDir, TempDir) {
        let nas_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        // Create fake NAS structure
        let folder = nas_dir.path().join("March 2026");
        fs::create_dir_all(&folder).unwrap();
        for i in 0..5 {
            let path = folder.join(format!("IMG_{:04}.jpg", i));
            fs::write(&path, vec![b'x'; 1000]).unwrap();
        }

        let db = CacheDB::open(std::path::Path::new(":memory:")).unwrap();
        let engine = SyncEngine::new(
            nas_dir.path().to_path_buf(),
            cache_dir.path().to_path_buf(),
            db,
            3000, // Only room for 3 files
        );

        (engine, nas_dir, cache_dir)
    }

    #[test]
    fn test_scan_nas() {
        let (engine, _nas, _cache) = setup();
        let files = engine.scan_nas();
        assert_eq!(files.len(), 5);
        assert!(files.iter().all(|f| f.rel_path.starts_with("March 2026/")));
    }

    #[test]
    fn test_sync_caches_newest() {
        let (engine, _nas, _cache) = setup();
        engine.sync();
        assert!(engine.db.total_size().unwrap() <= 3000);
        assert_eq!(engine.db.all_cached_paths().unwrap().len(), 3);
    }

    #[test]
    fn test_sync_creates_local_files() {
        let (engine, _nas, cache) = setup();
        engine.sync();
        for path in engine.db.all_cached_paths().unwrap() {
            assert!(cache.path().join(&path).exists());
        }
    }

    #[test]
    fn test_eviction() {
        let (engine, nas, _cache) = setup();
        engine.sync();
        // Add new file to NAS
        let new_file = nas.path().join("March 2026/IMG_NEW.jpg");
        fs::write(&new_file, vec![b'y'; 1000]).unwrap();
        // Set mtime to future
        filetime::set_file_mtime(&new_file, filetime::FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
        // Re-sync
        engine.sync();
        let cached = engine.db.all_cached_paths().unwrap();
        assert!(cached.contains("March 2026/IMG_NEW.jpg"));
        assert!(engine.db.total_size().unwrap() <= 3000);
    }
}
