// src/sync.rs
use crate::cache_db::CacheDB;
use crate::nas::NasHealth;
use log::{debug, info, warn};
use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use walkdir::WalkDir;

/// Bounds a single NAS reachability probe from the flush worker.
const FLUSH_NAS_OP_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the flush worker re-checks NAS reachability.
const FLUSH_NAS_PROBE_INTERVAL: Duration = Duration::from_secs(5);

const PHOTO_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "dng", "raw", "tiff", "tif", "cr2", "nef", "arw",
    "aae", "xmp", "mov",
];

/// Whether `nas_path` is a real (network) NAS mount rather than a dropped
/// mount's local underlay. Gates every destructive/write NAS operation so we
/// never strand data on, or purge the cache because of, a local underlay.
///
/// In production this is `crate::nas::is_network_mount`. Under `cargo test` it
/// defaults to `true` so the worker/cleanup logic can be exercised against local
/// temp dirs (unit tests can't create real NFS mounts); the real check is
/// covered by `nas::tests`.
#[cfg(not(test))]
fn nas_is_mounted(path: &Path) -> bool {
    crate::nas::is_network_mount(path)
}
#[cfg(test)]
fn nas_is_mounted(_path: &Path) -> bool {
    true
}

fn is_photo(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        // Skip macOS resource fork / AppleDouble files
        if name.starts_with("._") || name == ".DS_Store" {
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

/// Result of a successful safe_copy: size in bytes and SHA-256 hex digest.
pub struct CopyResult {
    pub size: u64,
    pub checksum: String,
}

/// The temp path a `safe_copy` writes to for `dst`: the destination name with a
/// `.tmp` suffix appended (e.g. `DSC_0918.dng` -> `DSC_0918.dng.tmp`). Appending
/// keeps it unique per destination — `with_extension("tmp")` would map both
/// `DSC_0918.dng` and a sibling `DSC_0918.jpg` (RAW+JPEG pairs) to the same
/// `DSC_0918.tmp` and let concurrent flushes clobber each other.
fn tmp_path(dst: &Path) -> PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    dst.with_file_name(name)
}

/// The macOS AppleDouble sidecar (`._name`) that shadows `p`, if `p` has a name.
fn appledouble_sibling(p: &Path) -> Option<PathBuf> {
    let name = p.file_name()?.to_str()?;
    Some(p.with_file_name(format!("._{}", name)))
}

/// Remove a temp file and any AppleDouble sidecar beside it, ignoring errors.
fn remove_tmp(tmp: &Path) {
    let _ = fs::remove_file(tmp);
    if let Some(sidecar) = appledouble_sibling(tmp) {
        let _ = fs::remove_file(sidecar);
    }
}

/// Copy a file safely: stream to a temp file, verify size, then atomically rename
/// to the final destination. Returns size and SHA-256 (of the bytes written) on
/// success. On failure, the temp file (and its AppleDouble sidecar) are cleaned
/// up and no partial file is left at `dst`.
///
/// We stream the copy by hand rather than calling `fs::copy`. On macOS `fs::copy`
/// uses `copyfile(COPYFILE_ALL)`, which replicates the source's extended
/// attributes (e.g. `com.apple.provenance`) onto the destination. An NFS mount
/// has no native xattr support, so macOS spills those into an AppleDouble `._`
/// sidecar file next to every copy — one turd per photo, forever, and never
/// reaped. A plain data copy sets no xattrs, so no `._` file is created. We hash
/// the bytes as we write them, which also avoids reading the freshly-written file
/// back over the (frequently timing-out) NAS just to checksum it.
fn safe_copy(src: &Path, dst: &Path) -> io::Result<CopyResult> {
    let tmp = tmp_path(dst);

    let src_size = fs::metadata(src)?.len();

    let mut reader = File::open(src)?;
    let mut writer = match File::create(&tmp) {
        Ok(w) => w,
        Err(e) => {
            remove_tmp(&tmp);
            return Err(e);
        }
    };

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                drop(writer);
                remove_tmp(&tmp);
                return Err(e);
            }
        };
        if let Err(e) = writer.write_all(&buf[..n]) {
            drop(writer);
            remove_tmp(&tmp);
            return Err(e);
        }
        hasher.update(&buf[..n]);
        written += n as u64;
    }
    if let Err(e) = writer.flush() {
        drop(writer);
        remove_tmp(&tmp);
        return Err(e);
    }
    drop(writer);

    // Verify the temp on the NAS is exactly the size we streamed (catches a
    // truncated write from a mid-copy timeout before we commit the rename).
    match fs::metadata(&tmp) {
        Ok(m) if m.len() == written && written == src_size => {}
        Ok(m) => {
            remove_tmp(&tmp);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "size mismatch: source {} bytes, wrote {} bytes, temp is {} bytes",
                    src_size, written, m.len()
                ),
            ));
        }
        Err(e) => {
            remove_tmp(&tmp);
            return Err(e);
        }
    }

    let checksum = format!("{:x}", hasher.finalize());

    // Atomic rename to final destination
    if let Err(e) = fs::rename(&tmp, dst) {
        remove_tmp(&tmp);
        return Err(e);
    }

    Ok(CopyResult { size: written, checksum })
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
        checksum: String,
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

        // Copy the file safely (temp file + size verify + checksum + atomic rename)
        match safe_copy(path, &dst) {
            Ok(result) => {
                debug!("Cached: {} ({:.1} MB, sha256:{})", rel_path, result.size as f64 / 1e6, &result.checksum[..8]);
                pending.push(PendingFile { rel_path, size: result.size, mtime, checksum: result.checksum });
                total_size += result.size;
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
        db.add(&file.rel_path, file.size, file.mtime, Some(&file.checksum)).ok();
    }

    // Only mark directory as fully cached if there were photos and all succeeded
    if cached_count == 0 && skipped_count == 0 {
        debug!("Directory has no photos to cache: {}", dir_rel);
    } else if failed {
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
/// Skips directories that are protected or have pending writes.
/// Returns the list of evicted directory names.
pub fn evict_lru(
    cache_dir: &Path,
    db: &CacheDB,
    max_cache_bytes: u64,
    protect_dir: Option<&str>,
) -> Vec<String> {
    let mut evicted = Vec::new();
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
        evicted.push(victim.dir_path.clone());
        let _ = db.remove_dir(&victim.dir_path);
    }
    evicted
}

/// Clean up stale state on startup:
/// 0. Remove leftover .tmp files from interrupted safe_copy operations
/// 1. Directories on disk without a DB entry (interrupted cache)
/// 2. DB entries whose files don't exist on disk (stale from old runs)
/// True only when the NAS *positively confirms* a directory is gone: its parent
/// lists successfully and the directory itself reports ENOENT. Any timeout / IO
/// error (soft mount on an unreachable NAS) returns false, so a network blip is
/// never mistaken for a deletion. Used to gate destructive cache invalidation.
fn nas_dir_confirmed_gone(nas_dir: &Path) -> bool {
    match fs::metadata(nas_dir) {
        Ok(_) => false, // still present
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Only trust ENOENT if the NAS is actually serving the parent dir.
            match nas_dir.parent() {
                Some(parent) => fs::read_dir(parent).is_ok(),
                None => false,
            }
        }
        Err(_) => false, // timeout / IO error → assume present, do not purge
    }
}

pub fn cleanup_stale_state(nas_path: &Path, cache_dir: &Path, db: &CacheDB) {
    if !cache_dir.is_dir() {
        return;
    }

    // 0. Remove leftover .tmp files from interrupted copies
    let mut tmp_removed = 0u32;
    for entry in WalkDir::new(cache_dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            if let Some(ext) = entry.path().extension() {
                if ext == "tmp" {
                    let _ = fs::remove_file(entry.path());
                    tmp_removed += 1;
                }
            }
        }
    }
    if tmp_removed > 0 {
        info!("Removed {} leftover .tmp files from interrupted copies", tmp_removed);
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
            let is_tracked = db.is_dir_cached(&dir_name).unwrap_or(true)
                || db.has_cached_children(&dir_name).unwrap_or(true);
            if !is_tracked {
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

    // 3. Remove cached files that no longer exist on NAS (skip pending writes).
    //
    //    SAFETY: only purge when the NAS *positively* reports the file gone
    //    (ENOENT) AND its parent directory is currently listable. A soft NFS
    //    mount returns timeouts/IO errors (not ENOENT) when the NAS is
    //    unreachable, and `Path::exists()` collapses "timed out" into `false` —
    //    so the old code treated a network blip as mass deletion and destroyed
    //    the cache. Here, any uncertainty means we keep the cached copy.
    //
    //    Also require the NAS to be a real network mount: against a dropped
    //    mount's local underlay every file would look ENOENT and get purged.
    if nas_path.is_dir() && nas_is_mounted(nas_path) {
        let cached_paths = db.all_cached_paths().unwrap_or_default();
        let pending = db.all_pending_writes().unwrap_or_default();
        let pending_set: std::collections::HashSet<&str> = pending.iter().map(|s| s.as_str()).collect();
        let mut nas_removed = 0u32;
        let mut kept_uncertain = 0u32;
        // Memoize NAS parent-directory reachability to avoid re-listing per file.
        let mut parent_listable: HashMap<PathBuf, bool> = HashMap::new();
        for path in &cached_paths {
            // Don't remove files that are pending NAS write — they're supposed to be local-only
            if pending_set.contains(path.as_str()) {
                debug!("Skipping pending write during cleanup: {}", path);
                continue;
            }
            let nas_file = nas_path.join(path);
            match fs::metadata(&nas_file) {
                Ok(_) => {} // still present on NAS
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // ENOENT is only trustworthy if the NAS is actually serving
                    // this subtree; confirm the parent dir lists before purging.
                    let listable = match nas_file.parent() {
                        Some(parent) => *parent_listable
                            .entry(parent.to_path_buf())
                            .or_insert_with(|| fs::read_dir(parent).is_ok()),
                        None => false,
                    };
                    if listable {
                        info!("File deleted from NAS, removing from cache: {}", path);
                        let cache_file = cache_dir.join(path);
                        if cache_file.exists() {
                            let _ = fs::remove_file(&cache_file);
                        }
                        let _ = db.remove(path);
                        nas_removed += 1;
                    } else {
                        kept_uncertain += 1;
                    }
                }
                Err(_) => {
                    // Timeout / IO error: NAS is unreliable right now. Keep the
                    // cached copy rather than risk purging live data.
                    kept_uncertain += 1;
                }
            }
        }
        if nas_removed > 0 {
            info!("Removed {} cached files confirmed deleted from NAS", nas_removed);
        }
        if kept_uncertain > 0 {
            warn!(
                "Kept {} cached files whose NAS status was unreachable/uncertain — not purging during NAS flakiness",
                kept_uncertain
            );
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
    /// Receive directory names found to have no photos.
    empty_rx: Mutex<mpsc::Receiver<String>>,
    /// Receive directory names that were evicted.
    evicted_rx: Mutex<mpsc::Receiver<String>>,
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
        let (empty_tx, empty_rx) = mpsc::channel::<String>();
        let (evicted_tx, evicted_rx) = mpsc::channel::<String>();

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

                // Notify: either cached successfully or empty (no photos)
                if db_guard.is_dir_cached(&dir_rel).unwrap_or(false) {
                    let _ = completed_tx.send(dir_rel.clone());
                } else {
                    let _ = empty_tx.send(dir_rel.clone());
                }

                // Evict LRU dirs if over budget (protect the one we just cached)
                for evicted_dir in evict_lru(&cache_dir, &db_guard, max_cache_bytes, Some(&dir_rel)) {
                    let _ = evicted_tx.send(evicted_dir);
                }
            }
            info!("Cache worker shutting down");
        });

        CacheWorker {
            tx,
            completed_rx: Mutex::new(completed_rx),
            empty_rx: Mutex::new(empty_rx),
            evicted_rx: Mutex::new(evicted_rx),
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

    /// Drain directories that were evicted from cache.
    pub fn drain_evicted(&self) -> Vec<String> {
        let rx = self.evicted_rx.lock().unwrap();
        let mut dirs = Vec::new();
        while let Ok(dir) = rx.try_recv() {
            dirs.push(dir);
        }
        dirs
    }

    /// Drain directories found to have no photos.
    pub fn drain_empty(&self) -> Vec<String> {
        let rx = self.empty_rx.lock().unwrap();
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
    /// Directories invalidated because they no longer exist on NAS.
    invalidated_rx: Mutex<mpsc::Receiver<String>>,
}

impl WriteFlushWorker {
    pub fn spawn(
        nas_path: PathBuf,
        cache_dir: PathBuf,
        db: Arc<Mutex<CacheDB>>,
        flush_interval: Duration,
        max_cache_bytes: u64,
    ) -> Self {
        let (flushed_tx, flushed_rx) = mpsc::channel::<String>();
        let (invalidated_tx, invalidated_rx) = mpsc::channel::<String>();

        let handle = thread::spawn(move || {
            // The flush worker's own reachability breaker. It gates writes exactly
            // as the FUSE reads are gated: don't touch the NAS unless it's a real
            // mount AND currently responsive. Hammering a timing-out NAS just
            // leaves a trail of partial `.tmp` files that can't be cleaned up.
            let nas = NasHealth::spawn(
                nas_path.clone(),
                FLUSH_NAS_OP_TIMEOUT,
                FLUSH_NAS_PROBE_INTERVAL,
            );
            let mut cycle_count = 0u64;
            let mut retry_counts: HashMap<String, u32> = HashMap::new();
            const MAX_BACKOFF_CYCLES: u32 = 64; // cap at ~5 min with 5s interval
            loop {
                thread::sleep(flush_interval);
                cycle_count += 1;

                // Collect pending paths while holding the lock briefly
                let pending = {
                    let db_guard = db.lock().unwrap();
                    db_guard.all_pending_writes().unwrap_or_default()
                };
                if pending.is_empty() {
                    retry_counts.clear();
                    continue;
                }

                // Only flush when the NAS is a real mount AND currently responsive
                // (the breaker folds in `is_network_mount`). This defers writes
                // during an outage instead of repeatedly writing partial `.tmp`
                // files to a timing-out NAS — and never strands data in a dropped
                // mount's local underlay. Files stay pending and safe in cache.
                if !nas.is_up() {
                    debug!(
                        "NAS unreachable — deferring {} pending flush(es) (avoids partial .tmp writes)",
                        pending.len()
                    );
                    continue;
                }

                // Clean up retry state for paths no longer pending
                retry_counts.retain(|k, _| pending.contains(k));

                let mut attempted = 0u32;
                let mut any_succeeded = false;
                for rel_path in &pending {
                    // Exponential backoff: skip if not enough cycles have passed
                    let retries = retry_counts.get(rel_path).copied().unwrap_or(0);
                    if retries > 0 && !any_succeeded {
                        // If a file already succeeded this cycle, network is back —
                        // skip backoff and retry everything immediately
                        let backoff = (1u64 << retries.min(6)).min(MAX_BACKOFF_CYCLES as u64);
                        if cycle_count % backoff != 0 {
                            continue;
                        }
                    }

                    let src = cache_dir.join(rel_path);
                    let dst = nas_path.join(rel_path);

                    if !src.exists() {
                        let db_guard = db.lock().unwrap();
                        db_guard.remove_pending_write(rel_path).ok();
                        debug!("Pending write removed (file gone): {}", rel_path);
                        let _ = flushed_tx.send(rel_path.clone());
                        retry_counts.remove(rel_path);
                        continue;
                    }

                    if let Some(parent) = dst.parent() {
                        fs::create_dir_all(parent).ok();
                    }

                    // Clean up any partial temp (and its AppleDouble sidecar)
                    // left on the NAS by a previous failed attempt.
                    remove_tmp(&tmp_path(&dst));

                    // Safe copy: temp file + size verify + checksum + atomic rename
                    match safe_copy(&src, &dst) {
                        Ok(result) => {
                            let db_guard = db.lock().unwrap();
                            db_guard.remove_pending_write(rel_path).ok();
                            info!("Flushed to NAS: {} ({:.1} MB, sha256:{})", rel_path, result.size as f64 / 1e6, &result.checksum[..8]);
                            let _ = flushed_tx.send(rel_path.clone());
                            retry_counts.remove(rel_path);
                            any_succeeded = true;
                        }
                        Err(e) => {
                            let count = retry_counts.entry(rel_path.clone()).or_insert(0);
                            *count = (*count + 1).min(MAX_BACKOFF_CYCLES);
                            let backoff = (1u64 << (*count).min(6)).min(MAX_BACKOFF_CYCLES as u64);
                            warn!("Failed to flush to NAS {}: {} (retry #{}, next in ~{}s)", rel_path, e, count, backoff * flush_interval.as_secs());
                        }
                    }
                    attempted += 1;
                }
                if any_succeeded {
                    // Network is back — reset all backoff so remaining files retry next cycle
                    retry_counts.clear();
                }
                if attempted > 0 {
                    info!("Flush cycle: attempted {} of {} pending writes", attempted, pending.len());
                }

                // After flushing, check if cache is over budget and evict if needed
                {
                    let db_guard = db.lock().unwrap();
                    evict_lru(&cache_dir, &db_guard, max_cache_bytes, None);
                }

                // Every ~60 seconds, verify cached directories still exist on NAS.
                // SAFETY: only invalidate on a *confirmed* removal — the NAS
                // parent must be listable and report ENOENT for this dir. A soft
                // NFS mount returns timeouts when unreachable; treating those as
                // removals would wipe live cached directories during a blip.
                // And never run this against a local underlay (mount dropped),
                // where every NAS dir would falsely look deleted.
                if cycle_count % 12 == 0 && nas.is_up() {
                    let db_guard = db.lock().unwrap();
                    if let Ok(dirs) = db_guard.lru_directories() {
                        for dir in &dirs {
                            let nas_dir = nas_path.join(&dir.dir_path);
                            if nas_dir_confirmed_gone(&nas_dir) {
                                info!("Directory removed from NAS, invalidating cache: {}", dir.dir_path);
                                let cache_path = cache_dir.join(&dir.dir_path);
                                if cache_path.is_dir() {
                                    let _ = fs::remove_dir_all(&cache_path);
                                }
                                let _ = db_guard.remove_dir(&dir.dir_path);
                                let _ = invalidated_tx.send(dir.dir_path.clone());
                            }
                        }
                    }
                }
            }
        });

        WriteFlushWorker {
            _handle: handle,
            flushed_rx: Mutex::new(flushed_rx),
            invalidated_rx: Mutex::new(invalidated_rx),
        }
    }

    /// Drain directories invalidated by NAS-side changes.
    pub fn drain_invalidated(&self) -> Vec<String> {
        let rx = self.invalidated_rx.lock().unwrap();
        let mut dirs = Vec::new();
        while let Ok(dir) = rx.try_recv() {
            dirs.push(dir);
        }
        dirs
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
    fn test_tmp_path_appends_suffix() {
        // Append, don't replace the extension: a RAW+JPEG pair sharing a stem
        // must not collide on one temp name.
        assert_eq!(tmp_path(Path::new("/a/DSC_0918.dng")), Path::new("/a/DSC_0918.dng.tmp"));
        assert_eq!(tmp_path(Path::new("/a/DSC_0918.jpg")), Path::new("/a/DSC_0918.jpg.tmp"));
        assert_ne!(tmp_path(Path::new("/a/DSC_0918.dng")), tmp_path(Path::new("/a/DSC_0918.jpg")));
    }

    #[test]
    fn test_safe_copy_roundtrip_and_no_readback_needed() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.dng");
        let dst = dir.path().join("sub/dst.dng");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        let data = vec![b'z'; 200_000]; // spans multiple 256 KiB-buffer reads' boundary logic
        fs::write(&src, &data).unwrap();

        let result = safe_copy(&src, &dst).unwrap();
        assert_eq!(result.size, data.len() as u64);
        assert_eq!(fs::read(&dst).unwrap(), data);
        // Checksum is the SHA-256 of the copied bytes.
        let mut h = Sha256::new();
        h.update(&data);
        assert_eq!(result.checksum, format!("{:x}", h.finalize()));
        // No temp file is left behind on success.
        assert!(!tmp_path(&dst).exists());
    }

    #[test]
    fn test_safe_copy_cleans_temp_and_sidecar_on_failure() {
        let dir = TempDir::new().unwrap();
        let dst = dir.path().join("dst.dng");
        // Missing source => copy fails; no temp or sidecar should survive.
        let err = safe_copy(&dir.path().join("does_not_exist.dng"), &dst);
        assert!(err.is_err());
        assert!(!tmp_path(&dst).exists());
        assert!(!dst.exists());

        // remove_tmp also reaps a stray AppleDouble sidecar left by an older run.
        let tmp = tmp_path(&dst);
        fs::write(&tmp, b"partial").unwrap();
        fs::write(appledouble_sibling(&tmp).unwrap(), b"\x00\x05\x16\x07").unwrap();
        remove_tmp(&tmp);
        assert!(!tmp.exists());
        assert!(!appledouble_sibling(&tmp).unwrap().exists());
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
        db.add("March 2026/ghost.jpg", 100, 1.0, None).unwrap();

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
        db.add("March 2026/deleted_from_nas.jpg", 100, 1.0, None).unwrap();

        // Also create a pending write file (also not on NAS — that's expected)
        fs::write(dir.join("pending.jpg"), b"new").unwrap();
        db.add("March 2026/pending.jpg", 50, 2.0, None).unwrap();
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
    fn test_nas_dir_confirmed_gone() {
        let nas = TempDir::new().unwrap();
        // Present directory → not gone.
        let present = nas.path().join("2026/April");
        fs::create_dir_all(&present).unwrap();
        assert!(!nas_dir_confirmed_gone(&present));
        // Genuinely removed, parent (2026) still lists → confirmed gone.
        let removed = nas.path().join("2026/Deleted");
        assert!(nas_dir_confirmed_gone(&removed));
        // Whole subtree unreachable (parent dir absent → stand-in for a timeout)
        // → NOT confirmed gone, cache must be kept.
        let unreachable = nas.path().join("NoSuchParent/Turkey");
        assert!(!nas_dir_confirmed_gone(&unreachable));
    }

    #[test]
    fn test_cleanup_keeps_cache_when_nas_subtree_unreachable() {
        // Regression: a flaky/soft NFS mount makes NAS reads fail (parent dir not
        // listable). Cleanup must NOT treat that as "deleted from NAS" and purge
        // the cache — otherwise a network blip destroys cached data.
        let nas = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();

        // Cached file present on disk + DB...
        let dir = cache.path().join("2025/Turkey");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("R0000254.DNG"), b"data").unwrap();
        db.add("2025/Turkey/R0000254.DNG", 100, 1.0, None).unwrap();
        db.touch_dir("2025/Turkey", 100).unwrap();

        // ...but the NAS root exists while the file's subtree (2025/Turkey) is
        // NOT present — i.e. the parent dir does not list (stand-in for a timeout).
        // NAS root must be a dir so step 3 runs at all.
        assert!(nas.path().is_dir());

        cleanup_stale_state(nas.path(), cache.path(), &db);

        assert!(db.get("2025/Turkey/R0000254.DNG").unwrap().is_some(),
            "cache DB entry must be kept when NAS subtree is unreachable");
        assert!(cache.path().join("2025/Turkey/R0000254.DNG").exists(),
            "cached file must be kept when NAS subtree is unreachable");
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

        let _size = cache_directory(nas.path(), cache.path(), "Bad Dir", &db);

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
            db_guard.add("March 2026/new_photo.jpg", 10, 1.0, None).unwrap();
            db_guard.add_pending_write("March 2026/new_photo.jpg").unwrap();
        }

        // Create NAS dir structure
        fs::create_dir_all(nas.path().join("March 2026")).unwrap();

        let worker = WriteFlushWorker::spawn(
            nas.path().to_path_buf(),
            cache.path().to_path_buf(),
            db.clone(),
            Duration::from_millis(50),
            u64::MAX, // no eviction limit in test
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
