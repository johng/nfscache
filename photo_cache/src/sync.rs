// src/sync.rs
use crate::cache_db::CacheDB;
use log::{debug, info, warn};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use walkdir::WalkDir;

const PHOTO_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "dng", "raw", "tiff", "tif", "cr2", "nef", "arw",
    "aae", "xmp", "mov",
];

fn is_photo(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        // Skip macOS resource fork / AppleDouble files
        if name.starts_with("._") {
            return false;
        }
        // Skip Synology metadata
        if name == "@eaDir" || name.contains("@Syno") {
            return false;
        }
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| PHOTO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Cache a single directory from NAS to local cache.
/// Returns the total size of files cached, or 0 if cancelled/failed.
/// Files are copied first, then recorded in the DB atomically at the end.
/// This ensures a partial cache from an interrupted run won't be marked as complete.
pub fn cache_directory(
    nas_path: &Path,
    cache_dir: &Path,
    dir_rel: &str,
    db: &CacheDB,
) -> u64 {
    let src_dir = nas_path.join(dir_rel);
    if !src_dir.is_dir() {
        return 0;
    }

    info!("Caching directory: {}", dir_rel);

    struct PendingFile {
        rel_path: String,
        size: u64,
        mtime: f64,
    }

    let mut pending: Vec<PendingFile> = Vec::new();
    let mut total_size = 0u64;
    let mut skipped_count = 0u32;
    let mut failed = false;

    for entry in WalkDir::new(&src_dir).max_depth(1).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_photo(path) {
            continue;
        }
        let rel_path = path.strip_prefix(nas_path).unwrap().to_string_lossy().to_string();
        let dst = cache_dir.join(&rel_path);

        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified()
            .unwrap_or(std::time::UNIX_EPOCH)
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        // Skip if already cached with same mtime
        if let Ok(Some(cached)) = db.get(&rel_path) {
            if cached.mtime == mtime && dst.exists() {
                total_size += cached.size;
                skipped_count += 1;
                continue;
            }
        }

        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).ok();
        }

        // Copy the file
        match fs::copy(path, &dst) {
            Ok(size) => {
                debug!("Cached: {} ({:.1} MB)", rel_path, size as f64 / 1e6);
                pending.push(PendingFile { rel_path, size, mtime });
                total_size += size;
            }
            Err(e) => {
                warn!("Failed to cache {}: {}", rel_path, e);
                failed = true;
            }
        }
    }

    // Record files in DB
    let cached_count = pending.len();
    for file in &pending {
        db.add(&file.rel_path, file.size, file.mtime).ok();
    }

    // Only mark directory as fully cached if all files succeeded
    if failed {
        warn!(
            "Directory partially cached: {} — {} files cached, {} skipped, some failed",
            dir_rel, cached_count, skipped_count
        );
    } else {
        db.touch_dir(dir_rel, total_size).ok();
        info!(
            "Directory cached: {} — {} files cached, {} skipped, {:.1} MB total",
            dir_rel, cached_count, skipped_count, total_size as f64 / 1e6
        );
    }
    total_size
}

/// Evict least-recently-accessed directories until total cache is under the limit.
/// Skips directories that are protected, have pending writes, or have open file handles.
pub fn evict_lru(
    cache_dir: &Path,
    db: &CacheDB,
    max_cache_bytes: u64,
    protect_dir: Option<&str>,
) {
    let initial_total = db.total_size().unwrap_or(0);
    if initial_total > max_cache_bytes {
        info!(
            "Cache over budget: {:.1} MB / {:.1} MB — evicting LRU directories",
            initial_total as f64 / 1e6, max_cache_bytes as f64 / 1e6
        );
    }

    // Collect dirs that have pending writes — these cannot be evicted
    let pending = db.all_pending_writes().unwrap_or_default();
    let dirs_with_pending: std::collections::HashSet<String> = pending.iter()
        .filter_map(|p| p.split('/').next().map(|s| s.to_string()))
        .collect();

    loop {
        let total = db.total_size().unwrap_or(0);
        if total <= max_cache_bytes {
            break;
        }

        let dirs = match db.lru_directories() {
            Ok(d) => d,
            Err(_) => break,
        };

        // Find the oldest directory that is safe to evict
        let victim = dirs.iter().find(|d| {
            let is_protected = protect_dir.map_or(false, |p| d.dir_path == p);
            let has_pending = dirs_with_pending.contains(&d.dir_path);
            !is_protected && !has_pending
        });

        let victim = match victim {
            Some(v) => v,
            None => {
                warn!("Cannot evict any directories (all protected or have pending writes)");
                break;
            }
        };

        info!("Evicting directory: {} ({} bytes)", victim.dir_path, victim.total_size);

        // Remove cached files from disk
        let dir_on_disk = cache_dir.join(&victim.dir_path);
        if dir_on_disk.is_dir() {
            let _ = fs::remove_dir_all(&dir_on_disk);
        }

        // Remove from DB
        let _ = db.remove_dir(&victim.dir_path);
    }
}

/// Clean up stale state on startup:
/// 1. Directories on disk without a DB entry (interrupted cache)
/// 2. DB entries whose files don't exist on disk (stale from old runs)
pub fn cleanup_stale_state(nas_path: &Path, cache_dir: &Path, db: &CacheDB) {
    if !cache_dir.is_dir() {
        return;
    }

    // Collect dirs that have pending writes — must not be deleted
    let pending = db.all_pending_writes().unwrap_or_default();
    let dirs_with_pending: std::collections::HashSet<String> = pending.iter()
        .filter_map(|p| p.split('/').next().map(|s| s.to_string()))
        .collect();

    // 1. Remove directories on disk that aren't tracked in DB (skip dirs with pending writes)
    if let Ok(read_dir) = fs::read_dir(cache_dir) {
        for entry in read_dir.filter_map(|e| e.ok()) {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if dirs_with_pending.contains(&dir_name) {
                debug!("Preserving untracked directory with pending writes: {}", dir_name);
                continue;
            }
            if !db.is_dir_cached(&dir_name).unwrap_or(true) {
                info!("Cleaning up untracked directory: {}", dir_name);
                let _ = fs::remove_dir_all(entry.path());
                let _ = db.remove_dir(&dir_name);
            }
        }
    }

    // 2. Remove DB entries whose files don't exist on disk
    let cached_paths = db.all_cached_paths().unwrap_or_default();
    let mut removed = 0u32;
    for path in &cached_paths {
        if !cache_dir.join(path).exists() {
            let _ = db.remove(path);
            removed += 1;
        }
    }
    if removed > 0 {
        info!("Removed {} stale DB entries (files missing from disk)", removed);
    }

    // 3. Remove cached files that no longer exist on NAS (skip pending writes)
    if nas_path.is_dir() {
        let cached_paths = db.all_cached_paths().unwrap_or_default();
        let pending = db.all_pending_writes().unwrap_or_default();
        let pending_set: std::collections::HashSet<&str> = pending.iter().map(|s| s.as_str()).collect();
        let mut nas_removed = 0u32;
        for path in &cached_paths {
            // Don't remove files that are pending NAS write — they're supposed to be local-only
            if pending_set.contains(path.as_str()) {
                debug!("Skipping pending write during cleanup: {}", path);
                continue;
            }
            if !nas_path.join(path).exists() {
                info!("File deleted from NAS, removing from cache: {}", path);
                let cache_file = cache_dir.join(path);
                if cache_file.exists() {
                    let _ = fs::remove_file(&cache_file);
                }
                let _ = db.remove(path);
                nas_removed += 1;
            }
        }
        if nas_removed > 0 {
            info!("Removed {} cached files deleted from NAS", nas_removed);
        }
    }

    // 4. Revalidate directory entries — remove dirs whose files are all gone
    if let Ok(dirs) = db.lru_directories() {
        for dir in &dirs {
            let dir_on_disk = cache_dir.join(&dir.dir_path);
            let has_files = dir_on_disk.is_dir()
                && fs::read_dir(&dir_on_disk)
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(false);
            if !has_files {
                info!("Removing empty cached directory: {}", dir.dir_path);
                let _ = fs::remove_dir_all(&dir_on_disk);
                let _ = db.remove_dir(&dir.dir_path);
            }
        }
    }
}

/// A handle to the background cache worker.
/// Send directory relative paths to trigger caching.
pub struct CacheWorker {
    tx: mpsc::Sender<String>,
    /// Receive directory names that have been fully cached.
    completed_rx: Mutex<mpsc::Receiver<String>>,
}

impl CacheWorker {
    /// Spawn a background thread that listens for directory cache requests.
    pub fn spawn(
        nas_path: PathBuf,
        cache_dir: PathBuf,
        db: Arc<Mutex<CacheDB>>,
        max_cache_bytes: u64,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<String>();
        let (completed_tx, completed_rx) = mpsc::channel::<String>();

        thread::spawn(move || {
            // Clean up any partial caches from previous interrupted runs
            {
                let db_guard = db.lock().unwrap();
                cleanup_stale_state(&nas_path, &cache_dir, &db_guard);
            }

            for dir_rel in rx {
                debug!("Cache request for directory: {}", dir_rel);

                let db_guard = db.lock().unwrap();

                // Cache the directory
                cache_directory(&nas_path, &cache_dir, &dir_rel, &db_guard);

                // Notify that this dir is now cached (if it was marked in DB)
                if db_guard.is_dir_cached(&dir_rel).unwrap_or(false) {
                    let _ = completed_tx.send(dir_rel.clone());
                }

                // Evict LRU dirs if over budget (protect the one we just cached)
                evict_lru(&cache_dir, &db_guard, max_cache_bytes, Some(&dir_rel));
            }
            info!("Cache worker shutting down");
        });

        CacheWorker {
            tx,
            completed_rx: Mutex::new(completed_rx),
        }
    }

    /// Request a directory to be cached. Non-blocking — returns immediately.
    pub fn request_cache(&self, dir_rel: String) {
        let _ = self.tx.send(dir_rel);
    }

    /// Drain any directories that have finished caching since the last call.
    pub fn drain_completed(&self) -> Vec<String> {
        let rx = self.completed_rx.lock().unwrap();
        let mut dirs = Vec::new();
        while let Ok(dir) = rx.try_recv() {
            dirs.push(dir);
        }
        dirs
    }
}

/// Background worker that flushes locally-written files to NAS.
pub struct WriteFlushWorker {
    _handle: thread::JoinHandle<()>,
    flushed_rx: Mutex<mpsc::Receiver<String>>,
}

impl WriteFlushWorker {
    pub fn spawn(
        nas_path: PathBuf,
        cache_dir: PathBuf,
        db: Arc<Mutex<CacheDB>>,
        flush_interval: Duration,
    ) -> Self {
        let (flushed_tx, flushed_rx) = mpsc::channel::<String>();

        let handle = thread::spawn(move || {
            loop {
                thread::sleep(flush_interval);

                // Collect pending paths while holding the lock briefly
                let pending = {
                    let db_guard = db.lock().unwrap();
                    db_guard.all_pending_writes().unwrap_or_default()
                };
                if pending.is_empty() {
                    continue;
                }

                info!("Flushing {} pending writes to NAS", pending.len());
                for rel_path in &pending {
                    let src = cache_dir.join(rel_path);
                    let dst = nas_path.join(rel_path);

                    if !src.exists() {
                        let db_guard = db.lock().unwrap();
                        db_guard.remove_pending_write(rel_path).ok();
                        debug!("Pending write removed (file gone): {}", rel_path);
                        let _ = flushed_tx.send(rel_path.clone());
                        continue;
                    }

                    if let Some(parent) = dst.parent() {
                        fs::create_dir_all(parent).ok();
                    }

                    // Copy without holding the DB lock
                    match fs::copy(&src, &dst) {
                        Ok(size) => {
                            let db_guard = db.lock().unwrap();
                            db_guard.remove_pending_write(rel_path).ok();
                            info!("Flushed to NAS: {} ({:.1} MB)", rel_path, size as f64 / 1e6);
                            let _ = flushed_tx.send(rel_path.clone());
                        }
                        Err(e) => {
                            warn!("Failed to flush to NAS {}: {} (will retry)", rel_path, e);
                        }
                    }
                }
            }
        });

        WriteFlushWorker {
            _handle: handle,
            flushed_rx: Mutex::new(flushed_rx),
        }
    }

    /// Drain flushed file paths to update in-memory state.
    pub fn drain_flushed(&self) -> Vec<String> {
        let rx = self.flushed_rx.lock().unwrap();
        let mut paths = Vec::new();
        while let Ok(p) = rx.try_recv() {
            paths.push(p);
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, TempDir, CacheDB) {
        let nas_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        // Create fake NAS structure
        let folder = nas_dir.path().join("March 2026");
        fs::create_dir_all(&folder).unwrap();
        for i in 0..5 {
            let path = folder.join(format!("IMG_{:04}.jpg", i));
            fs::write(&path, vec![b'x'; 1000]).unwrap();
        }

        let folder2 = nas_dir.path().join("April 2026");
        fs::create_dir_all(&folder2).unwrap();
        for i in 0..3 {
            let path = folder2.join(format!("IMG_{:04}.jpg", i));
            fs::write(&path, vec![b'y'; 1000]).unwrap();
        }

        let db = CacheDB::open(Path::new(":memory:")).unwrap();
        (nas_dir, cache_dir, db)
    }

    #[test]
    fn test_cache_directory() {
        let (nas, cache, db) = setup();
        let size = cache_directory(nas.path(), cache.path(), "March 2026", &db);
        assert_eq!(size, 5000);
        assert_eq!(db.all_cached_paths().unwrap().len(), 5);
        assert!(db.is_dir_cached("March 2026").unwrap());
        // Files exist on disk
        assert!(cache.path().join("March 2026/IMG_0000.jpg").exists());
    }

    #[test]
    fn test_cache_directory_skips_existing() {
        let (nas, cache, db) = setup();
        cache_directory(nas.path(), cache.path(), "March 2026", &db);
        // Cache again — should skip (same mtime)
        let size = cache_directory(nas.path(), cache.path(), "March 2026", &db);
        assert_eq!(size, 5000); // Still reports total size
    }

    #[test]
    fn test_evict_lru() {
        let (nas, cache, db) = setup();
        cache_directory(nas.path(), cache.path(), "March 2026", &db);
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache_directory(nas.path(), cache.path(), "April 2026", &db);

        // Evict with limit that only fits one dir (3000 bytes)
        evict_lru(cache.path(), &db, 3000, None);

        // March (oldest accessed) should be evicted
        assert!(!db.is_dir_cached("March 2026").unwrap());
        assert!(db.is_dir_cached("April 2026").unwrap());
        assert!(!cache.path().join("March 2026").exists());
    }

    #[test]
    fn test_evict_protects_dir() {
        let (nas, cache, db) = setup();
        cache_directory(nas.path(), cache.path(), "March 2026", &db);
        std::thread::sleep(std::time::Duration::from_millis(10));
        cache_directory(nas.path(), cache.path(), "April 2026", &db);

        // Evict with tiny limit, but protect March
        evict_lru(cache.path(), &db, 3000, Some("March 2026"));

        // April (not protected) should be evicted even though it's newer
        assert!(db.is_dir_cached("March 2026").unwrap());
        assert!(!db.is_dir_cached("April 2026").unwrap());
    }

    // --- is_photo tests ---

    #[test]
    fn test_is_photo_accepts_photo_extensions() {
        for ext in &["jpg", "jpeg", "png", "heic", "heif", "dng", "raw", "tiff", "tif", "cr2", "nef", "arw"] {
            let path = Path::new("test").join(format!("photo.{}", ext));
            assert!(is_photo(&path), "expected {} to be a photo", ext);
        }
    }

    #[test]
    fn test_is_photo_case_insensitive() {
        assert!(is_photo(Path::new("IMG_001.JPG")));
        assert!(is_photo(Path::new("photo.DNG")));
        assert!(is_photo(Path::new("photo.Heic")));
    }

    #[test]
    fn test_is_photo_rejects_non_photo() {
        assert!(!is_photo(Path::new("readme.txt")));
        assert!(!is_photo(Path::new("video.mp4")));
        assert!(!is_photo(Path::new("data.json")));
        assert!(!is_photo(Path::new("no_extension")));
    }

    #[test]
    fn test_is_photo_rejects_resource_forks() {
        assert!(!is_photo(Path::new("._IMG_001.jpg")));
        assert!(!is_photo(Path::new("dir/._photo.heic")));
    }

    #[test]
    fn test_is_photo_rejects_synology_metadata() {
        assert!(!is_photo(Path::new("@eaDir")));
        assert!(!is_photo(Path::new("@SynoResource")));
        assert!(!is_photo(Path::new("file@SynoExt.jpg")));
    }

    // --- cleanup_stale_state tests ---

    #[test]
    fn test_cleanup_removes_untracked_dir_on_disk() {
        let nas = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();

        // Create a directory in cache that isn't tracked in DB
        let untracked = cache.path().join("Stale Dir");
        fs::create_dir_all(&untracked).unwrap();
        fs::write(untracked.join("leftover.jpg"), b"data").unwrap();

        cleanup_stale_state(nas.path(), cache.path(), &db);

        assert!(!untracked.exists(), "untracked dir should be removed");
    }

    #[test]
    fn test_cleanup_removes_db_entry_with_no_file() {
        let nas = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();

        // Add DB entry but no actual file on disk
        db.add("March 2026/ghost.jpg", 100, 1.0).unwrap();

        cleanup_stale_state(nas.path(), cache.path(), &db);

        assert!(db.get("March 2026/ghost.jpg").unwrap().is_none(),
            "stale DB entry should be removed");
    }

    #[test]
    fn test_cleanup_removes_file_deleted_from_nas_but_preserves_pending() {
        let nas = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();

        // Create a cached file that exists on disk but NOT on NAS
        let dir = cache.path().join("March 2026");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("deleted_from_nas.jpg"), b"data").unwrap();
        db.add("March 2026/deleted_from_nas.jpg", 100, 1.0).unwrap();

        // Also create a pending write file (also not on NAS — that's expected)
        fs::write(dir.join("pending.jpg"), b"new").unwrap();
        db.add("March 2026/pending.jpg", 50, 2.0).unwrap();
        db.add_pending_write("March 2026/pending.jpg").unwrap();

        // Track the directory in DB so step 1 doesn't remove it as untracked
        db.touch_dir("March 2026", 150).unwrap();

        // NAS dir must exist for step 3 to run, but files are missing from it
        fs::create_dir_all(nas.path().join("March 2026")).unwrap();

        cleanup_stale_state(nas.path(), cache.path(), &db);

        // deleted_from_nas.jpg should be removed
        assert!(db.get("March 2026/deleted_from_nas.jpg").unwrap().is_none(),
            "file deleted from NAS should be removed from DB");
        assert!(!cache.path().join("March 2026/deleted_from_nas.jpg").exists(),
            "file deleted from NAS should be removed from disk");

        // pending.jpg should be preserved
        assert!(db.get("March 2026/pending.jpg").unwrap().is_some(),
            "pending write should be preserved in DB");
        assert!(cache.path().join("March 2026/pending.jpg").exists(),
            "pending write should be preserved on disk");
    }

    #[test]
    fn test_cleanup_removes_empty_directory_entries() {
        let nas = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();

        // Track a directory in DB but create it empty on disk
        db.touch_dir("Empty Dir", 0).unwrap();
        let empty = cache.path().join("Empty Dir");
        fs::create_dir_all(&empty).unwrap();

        cleanup_stale_state(nas.path(), cache.path(), &db);

        assert!(!db.is_dir_cached("Empty Dir").unwrap(),
            "empty directory entry should be removed from DB");
    }

    // --- cache_directory with failed copy ---

    #[test]
    fn test_cache_directory_failed_copy_not_marked_cached() {
        let nas = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();

        let src_dir = nas.path().join("Bad Dir");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("good.jpg"), b"data").unwrap();

        // Create a file that will fail to copy: make cache subdir a file (not a dir)
        // so create_dir_all for the parent fails and copy fails
        fs::write(cache.path().join("Bad Dir"), b"blocker").unwrap();

        let size = cache_directory(nas.path(), cache.path(), "Bad Dir", &db);

        // The directory should NOT be marked as cached since copy failed
        assert!(!db.is_dir_cached("Bad Dir").unwrap(),
            "directory with failed copies should not be marked as cached");
    }

    // --- WriteFlushWorker tests ---

    #[test]
    fn test_write_flush_worker_flushes_pending() {
        let nas = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();
        let db = Arc::new(Mutex::new(db));

        // Set up a file in cache with a pending write
        let dir = cache.path().join("March 2026");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("new_photo.jpg"), b"local data").unwrap();

        {
            let db_guard = db.lock().unwrap();
            db_guard.add("March 2026/new_photo.jpg", 10, 1.0).unwrap();
            db_guard.add_pending_write("March 2026/new_photo.jpg").unwrap();
        }

        // Create NAS dir structure
        fs::create_dir_all(nas.path().join("March 2026")).unwrap();

        let worker = WriteFlushWorker::spawn(
            nas.path().to_path_buf(),
            cache.path().to_path_buf(),
            db.clone(),
            Duration::from_millis(50),
        );

        // Wait for flush to happen
        std::thread::sleep(Duration::from_millis(200));

        // File should be copied to NAS
        assert!(nas.path().join("March 2026/new_photo.jpg").exists(),
            "file should be flushed to NAS");

        // Pending write should be removed from DB
        {
            let db_guard = db.lock().unwrap();
            assert!(db_guard.all_pending_writes().unwrap().is_empty(),
                "pending write should be cleared after flush");
        }

        // drain_flushed should report the flushed file
        let flushed = worker.drain_flushed();
        assert!(flushed.contains(&"March 2026/new_photo.jpg".to_string()),
            "drain_flushed should report flushed files");
    }
}
