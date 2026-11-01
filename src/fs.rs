// src/fs.rs — FUSE filesystem presenting a unified view of NAS + local cache
use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    RenameFlags, Request, TimeOrNow, WriteFlags,
};
use crate::cache_db::CacheDB;
use crate::nas::NasHealth;
use crate::sync::CacheWorker;
use crate::sync::WriteFlushWorker;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(30);
const DIR_LIST_CACHE_TTL: Duration = Duration::from_secs(30);
/// Max time a single FUSE-thread NAS call may block before the breaker trips.
const NAS_OP_TIMEOUT: Duration = Duration::from_secs(3);
/// Directory listings get longer: a cold listing of a big folder over a VPN can
/// legitimately take several seconds, and timing out turns a healthy NAS into an
/// EIO for the caller (e.g. photosync aborting). Roughly the soft-NFS give-up time.
const NAS_READDIR_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the background health thread re-checks NAS reachability.
const NAS_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const ROOT_INO: u64 = 1;
const FINDER_INFO_XATTR: &str = "com.apple.FinderInfo";
const FINDER_INFO_LEN: usize = 32;

// FinderInfo color label values for byte 9: color << 1 (bits 3:1)
// Verified mapping: 1=gray, 2=green, 3=purple, 4=blue, 5=yellow, 6=red, 7=orange
const FINDER_COLOR_GREEN: u8 = 2 << 1;  // 0x04
const FINDER_COLOR_ORANGE: u8 = 7 << 1; // 0x0E

fn make_finder_info(color_byte: u8) -> [u8; FINDER_INFO_LEN] {
    let mut info = [0u8; FINDER_INFO_LEN];
    info[9] = color_byte;
    info
}

/// Read a directory's entries as (name, is_dir) pairs. Runs on a NAS-I/O worker
/// thread (via NasHealth::run) so a hung mount can't block the FUSE thread.
fn read_dir_names(dir: &Path) -> std::io::Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push((name.to_string(), is_dir));
        }
    }
    Ok(out)
}

struct InodeMap {
    path_to_ino: HashMap<String, u64>,
    ino_to_path: HashMap<u64, String>,
    next_ino: u64,
}

impl InodeMap {
    fn new() -> Self {
        let mut map = InodeMap {
            path_to_ino: HashMap::new(),
            ino_to_path: HashMap::new(),
            next_ino: 2, // 1 is reserved for root
        };
        // Root directory is empty relative path
        map.path_to_ino.insert(String::new(), ROOT_INO);
        map.ino_to_path.insert(ROOT_INO, String::new());
        map
    }

    fn get_or_create(&mut self, rel_path: &str) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(rel_path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.path_to_ino.insert(rel_path.to_string(), ino);
        self.ino_to_path.insert(ino, rel_path.to_string());
        ino
    }

    fn get_path(&self, ino: u64) -> Option<&str> {
        self.ino_to_path.get(&ino).map(|s| s.as_str())
    }

    fn remove_path(&mut self, rel_path: &str) {
        if let Some(ino) = self.path_to_ino.remove(rel_path) {
            self.ino_to_path.remove(&ino);
        }
    }

    fn rename(&mut self, old_path: &str, new_path: &str) {
        if let Some(ino) = self.path_to_ino.remove(old_path) {
            self.ino_to_path.insert(ino, new_path.to_string());
            self.path_to_ino.insert(new_path.to_string(), ino);
        }
    }
}

struct OpenFileHandle {
    file: File,
    /// True if the file handle points to the cache copy (vs NAS directly)
    is_cached: bool,
    /// True if data was written through this handle
    dirty: bool,
}

/// Result of listing a directory.
enum DirListing {
    /// A complete, trustworthy listing. Sourced from a fully-cached directory, a
    /// successful live NAS read, or — during a NAS outage — the persisted tree
    /// metadata for a previously-seen directory.
    Complete(Vec<(String, FileType)>),
    /// The directory's contents are currently unknowable: the NAS is unreachable
    /// and we have neither a full cache nor any persisted metadata for it. The
    /// caller MUST surface this as an I/O error, never as an empty directory — a
    /// truncated listing silently corrupts anything that trusts it (dedup tools,
    /// backups, `rm -r`).
    Unavailable,
}

#[cfg(test)]
impl DirListing {
    fn into_entries(self) -> Vec<(String, FileType)> {
        match self {
            DirListing::Complete(e) => e,
            DirListing::Unavailable => Vec::new(),
        }
    }
}

pub struct NfsCacheFS {
    nas_path: PathBuf,
    cache_dir: PathBuf,
    inodes: Mutex<InodeMap>,
    file_handles: Mutex<HashMap<u64, OpenFileHandle>>,
    next_fh: Mutex<u64>,
    db: Option<Mutex<CacheDB>>,
    cache_worker: Option<CacheWorker>,
    flush_worker: Option<WriteFlushWorker>,
    /// In-memory set of fully cached directory names for fast lookups.
    cached_dirs: Mutex<HashSet<String>>,
    /// Directories known to have no photos (avoid repeated cache attempts).
    empty_dirs: Mutex<HashSet<String>>,
    /// Track last LRU touch time per directory to avoid DB writes on every open.
    last_dir_touch: Mutex<HashMap<String, Instant>>,
    /// In-memory set of files pending NAS write.
    pending_writes: Mutex<HashSet<String>>,
    /// Cached directory listings to avoid repeated NAS readdir calls.
    dir_list_cache: Mutex<HashMap<String, (Instant, Vec<(String, FileType)>)>>,
    /// Circuit breaker so FUSE callbacks never block on a hung NAS mount.
    /// Shared (Arc) with the background metadata crawler so both agree on
    /// reachability and never fight over a hung mount.
    nas: Arc<NasHealth>,
    uid: u32,
    gid: u32,
}

impl NfsCacheFS {
    pub fn new(
        nas_path: PathBuf,
        cache_dir: PathBuf,
        db: Option<CacheDB>,
        cache_worker: Option<CacheWorker>,
        flush_worker: Option<WriteFlushWorker>,
    ) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };

        let nas = Arc::new(NasHealth::spawn(nas_path.clone(), NAS_OP_TIMEOUT, NAS_PROBE_INTERVAL));

        // Pre-load cached directory names and pending writes from DB
        let mut initial_dirs = HashSet::new();
        let mut initial_pending = HashSet::new();
        if let Some(ref db) = db {
            if let Ok(dirs) = db.lru_directories() {
                for d in dirs {
                    initial_dirs.insert(d.dir_path);
                }
            }
            if let Ok(pending) = db.all_pending_writes() {
                for p in pending {
                    initial_pending.insert(p);
                }
            }
        }

        NfsCacheFS {
            nas_path,
            cache_dir,
            inodes: Mutex::new(InodeMap::new()),
            file_handles: Mutex::new(HashMap::new()),
            next_fh: Mutex::new(1),
            db: db.map(Mutex::new),
            cache_worker,
            flush_worker,
            cached_dirs: Mutex::new(initial_dirs),
            empty_dirs: Mutex::new(HashSet::new()),
            pending_writes: Mutex::new(initial_pending),
            dir_list_cache: Mutex::new(HashMap::new()),
            last_dir_touch: Mutex::new(HashMap::new()),
            nas,
            uid,
            gid,
        }
    }

    /// A shareable handle to the reachability breaker, for the background crawler.
    fn nas_handle(&self) -> Arc<NasHealth> {
        self.nas.clone()
    }

    /// Check if an individual file exists in the local cache.
    fn is_file_cached(&self, rel_path: &str) -> bool {
        if rel_path.is_empty() || !rel_path.contains('/') {
            return false;
        }
        let path = self.cache_dir.join(rel_path);
        path.is_file()
    }

    /// Check if a file has a pending write (not yet synced to NAS).
    fn is_pending_write(&self, rel_path: &str) -> bool {
        self.pending_writes.lock().unwrap().contains(rel_path)
    }

    /// Check if a path is fully cached locally.
    /// Uses in-memory set — no DB lock needed.
    fn is_cached(&self, rel_path: &str) -> bool {
        if rel_path.is_empty() {
            return false;
        }
        let dirs = self.cached_dirs.lock().unwrap();
        // Check if this path is itself a cached directory
        if dirs.contains(rel_path) {
            return true;
        }
        // Check if this file's parent directory is fully cached
        if let Some(dir) = Self::parent_dir(rel_path) {
            return dirs.contains(dir);
        }
        false
    }

    /// Extract the parent directory from a relative path (e.g., "March 2026/IMG.jpg" -> "March 2026", "2024/January/DSC.jpg" -> "2024/January")
    fn parent_dir(rel_path: &str) -> Option<&str> {
        rel_path.rfind('/').map(|pos| &rel_path[..pos]).filter(|s| !s.is_empty())
    }

    /// Trigger background caching of the directory containing this file.
    fn trigger_dir_cache(&self, rel_path: &str) {
        // Drain completed, evicted, empty dirs, and flushed writes
        if let Some(ref worker) = self.cache_worker {
            for dir in worker.drain_completed() {
                self.cached_dirs.lock().unwrap().insert(dir);
            }
            for dir in worker.drain_evicted() {
                self.cached_dirs.lock().unwrap().remove(&dir);
            }
            for dir in worker.drain_empty() {
                self.empty_dirs.lock().unwrap().insert(dir);
            }
        }
        if let Some(ref flush) = self.flush_worker {
            let flushed = flush.drain_flushed();
            if !flushed.is_empty() {
                let mut pending = self.pending_writes.lock().unwrap();
                for path in flushed {
                    pending.remove(&path);
                }
            }
            for dir in flush.drain_invalidated() {
                self.cached_dirs.lock().unwrap().remove(&dir);
            }
        }

        if let Some(dir) = Self::parent_dir(rel_path) {
            let already_cached = self.cached_dirs.lock().unwrap().contains(dir);
            let is_empty = self.empty_dirs.lock().unwrap().contains(dir);
            if already_cached {
                // Touch the dir to update LRU — but only once per minute to avoid
                // DB writes on every file open
                let should_touch = {
                    let mut last = self.last_dir_touch.lock().unwrap();
                    let now = Instant::now();
                    match last.get(dir) {
                        Some(t) if now.duration_since(*t) < Duration::from_secs(60) => false,
                        _ => { last.insert(dir.to_string(), now); true }
                    }
                };
                if should_touch {
                    if let Some(ref db) = self.db {
                        let db = db.lock().unwrap();
                        let _ = db.touch_dir_access(dir);
                    }
                }
            } else if !is_empty {
                debug!("Requesting cache for directory: {}", dir);
                if let Some(ref worker) = self.cache_worker {
                    worker.request_cache(dir.to_string());
                }
            }
        }
    }

    /// Record a file in the cache DB (if DB is available).
    fn db_add(&self, rel_path: &str, size: u64, mtime: f64) {
        if let Some(ref db) = self.db {
            let db = db.lock().unwrap();
            if let Err(e) = db.add(rel_path, size, mtime, None) {
                warn!("Failed to update cache DB for {}: {}", rel_path, e);
            }
        }
    }

    /// Remove a file from the cache DB.
    fn db_remove(&self, rel_path: &str) {
        if let Some(ref db) = self.db {
            let db = db.lock().unwrap();
            if let Err(e) = db.remove(rel_path) {
                warn!("Failed to remove {} from cache DB: {}", rel_path, e);
            }
        }
    }

    /// Rename a file in the cache DB (remove old, add new).
    fn db_rename(&self, old_path: &str, new_path: &str) {
        if let Some(ref db) = self.db {
            let db = db.lock().unwrap();
            let info = db.get(old_path).ok().flatten();
            let _ = db.remove(old_path);
            if let Some(entry) = info {
                let _ = db.add(new_path, entry.size, entry.mtime, entry.checksum.as_deref());
            }
        }
    }

    /// Resolve a relative path to the actual filesystem path.
    /// Checks cache first, then NAS. Skips NAS for files in cached directories.
    fn resolve(&self, rel_path: &str) -> Option<PathBuf> {
        if rel_path.is_empty() {
            if self.cache_dir.exists() {
                return Some(self.cache_dir.clone());
            }
            if self.nas.exists(&self.nas_path) {
                return Some(self.nas_path.clone());
            }
            return None;
        }
        let cached = self.cache_dir.join(rel_path);
        if cached.exists() {
            return Some(cached);
        }
        // If this file's directory is fully cached and it's not in cache, it doesn't exist
        if self.is_cached(rel_path) {
            return None;
        }
        // NAS access is bounded by the breaker — returns None fast if NAS is down.
        let nas = self.nas_path.join(rel_path);
        if self.nas.exists(&nas) {
            return Some(nas);
        }
        None
    }

    /// Resolve a relative path and return both the path and its metadata in one shot.
    /// Avoids the double-stat of resolve() + fs::metadata().
    fn resolve_with_meta(&self, rel_path: &str) -> Option<(PathBuf, std::fs::Metadata)> {
        if rel_path.is_empty() {
            if let Ok(m) = fs::metadata(&self.cache_dir) {
                return Some((self.cache_dir.clone(), m));
            }
            if let Some(m) = self.nas.metadata(&self.nas_path) {
                return Some((self.nas_path.clone(), m));
            }
            return None;
        }
        let cached = self.cache_dir.join(rel_path);
        if let Ok(m) = fs::metadata(&cached) {
            return Some((cached, m));
        }
        // If this file's directory is fully cached and it's not in cache, it doesn't exist
        if self.is_cached(rel_path) {
            return None;
        }
        // NAS access is bounded by the breaker — returns None fast if NAS is down.
        let nas = self.nas_path.join(rel_path);
        if let Some(m) = self.nas.metadata(&nas) {
            return Some((nas, m));
        }
        None
    }

    /// Build FileAttr from filesystem metadata
    fn make_attr(&self, ino: INodeNo, meta: &std::fs::Metadata) -> FileAttr {
        let kind = if meta.is_dir() {
            FileType::Directory
        } else if meta.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };

        let atime = meta.accessed().unwrap_or(UNIX_EPOCH);
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        let ctime = if meta.ctime() >= 0 {
            SystemTime::UNIX_EPOCH + Duration::from_secs(meta.ctime() as u64)
        } else {
            SystemTime::UNIX_EPOCH
        };
        let crtime = meta.created().unwrap_or(UNIX_EPOCH);

        // Ensure owner has rw access (NAS files may be owned by a different uid)
        let mut perm = meta.mode() as u16;
        if kind == FileType::Directory {
            perm |= 0o700; // rwx for owner on dirs
        } else {
            perm |= 0o600; // rw for owner on files
        }

        FileAttr {
            ino,
            size: meta.len(),
            blocks: meta.blocks(),
            atime,
            mtime,
            ctime,
            crtime,
            kind,
            perm,
            nlink: meta.nlink() as u32,
            uid: self.uid,
            gid: self.gid,
            rdev: meta.rdev() as u32,
            blksize: meta.blksize() as u32,
            flags: 0,
        }
    }

    /// List children of a directory by merging NAS + cache entries.
    /// Names to hide from directory listings (Synology metadata, macOS resource forks).
    fn is_hidden_entry(name: &str) -> bool {
        name == "@eaDir" || name == ".DS_Store" || name.starts_with("._") || name.contains("@Syno")
            // In-flight/partial copy temp files (from safe_copy) — never surface
            // them in listings or record them in the tree index.
            || name.ends_with(".tmp")
    }

    /// Temp files that should not be tracked as pending NAS writes.
    fn is_temp_file(name: &str) -> bool {
        name == ".DS_Store" || name.ends_with(".swp") || name.ends_with(".tmp")
            || name.ends_with("~") || name.starts_with(".~") || name.starts_with("~$")
    }

    fn read_dir_entries(dir: &Path, entries: &mut HashMap<String, FileType>) {
        if let Ok(read_dir) = fs::read_dir(dir) {
            for entry in read_dir.filter_map(|e| e.ok()) {
                if let Some(name) = entry.file_name().to_str() {
                    if Self::is_hidden_entry(name) {
                        continue;
                    }
                    let ft = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    entries.insert(name.to_string(), ft);
                }
            }
        }
    }

    /// Read NAS directory entries through the breaker so a hung mount can't
    /// block the FUSE thread. Merges into `entries`. Returns `true` if the NAS
    /// answered (the listing is authoritative), `false` if the NAS was down or
    /// the read timed out (nothing merged — caller must not trust `entries` as
    /// complete).
    fn read_nas_dir_entries(&self, dir: &Path, entries: &mut HashMap<String, FileType>) -> bool {
        let dir = dir.to_path_buf();
        let names = self.nas.run_with_timeout(NAS_READDIR_TIMEOUT, move || read_dir_names(&dir));
        match names {
            Some(names) => {
                for (name, is_dir) in names {
                    if Self::is_hidden_entry(&name) {
                        continue;
                    }
                    let ft = if is_dir { FileType::Directory } else { FileType::RegularFile };
                    entries.insert(name, ft);
                }
                true
            }
            None => false,
        }
    }

    fn list_dir(&self, rel_path: &str) -> DirListing {
        // Check in-memory cache first — only complete listings are ever stored here.
        {
            let cache = self.dir_list_cache.lock().unwrap();
            if let Some((when, entries)) = cache.get(rel_path) {
                if when.elapsed() < DIR_LIST_CACHE_TTL {
                    return DirListing::Complete(entries.clone());
                }
            }
        }

        let mut entries: HashMap<String, FileType> = HashMap::new();
        let dir_is_cached = self.is_cached(rel_path);

        // Skip NAS read if this directory is fully cached — serve from cache only.
        let mut nas_ok = false;
        if !dir_is_cached {
            let nas_dir = if rel_path.is_empty() {
                self.nas_path.clone()
            } else {
                self.nas_path.join(rel_path)
            };
            nas_ok = self.read_nas_dir_entries(&nas_dir, &mut entries);
        }

        // Local cache files are present regardless of NAS state — always merge.
        let cache_dir = if rel_path.is_empty() {
            self.cache_dir.clone()
        } else {
            self.cache_dir.join(rel_path)
        };
        Self::read_dir_entries(&cache_dir, &mut entries);

        // We have an authoritative, complete listing only if the directory is
        // fully cached locally or the NAS read *succeeded*. A failed read — for
        // ANY reason (a timeout that trips the breaker, OR a fast soft-mount EIO
        // that doesn't) — is never authoritative: `entries` then holds at most a
        // handful of individually-cached files, which must not be presented as
        // the whole directory. A successful *empty* read IS authoritative, since
        // nas_ok is true in that case.
        //
        // NB: an earlier version gated this on the breaker's `is_up()` flag. That
        // was wrong — a quick EIO leaves the breaker up, so truncated listings
        // slipped through and photosync re-imported already-present photos.
        let authoritative = dir_is_cached || nas_ok;

        if authoritative {
            let mut result: Vec<(String, FileType)> = entries.into_iter().collect();
            result.sort_by(|a, b| a.0.cmp(&b.0));

            // Persist the structure whenever we learned it fresh from the NAS, so
            // future outages can serve a complete listing of this directory.
            if nas_ok {
                if let Some(ref db) = self.db {
                    let rows: Vec<(String, bool)> = result.iter()
                        .map(|(n, ft)| (n.clone(), *ft == FileType::Directory))
                        .collect();
                    let db = db.lock().unwrap();
                    if let Err(e) = db.replace_dir_listing(rel_path, &rows) {
                        warn!("Failed to persist dir tree for {}: {}", rel_path, e);
                    }
                }
            }

            self.dir_list_cache.lock().unwrap()
                .insert(rel_path.to_string(), (Instant::now(), result.clone()));

            return DirListing::Complete(result);
        }

        // The NAS read failed and this directory isn't fully cached. Fall back to
        // the last-known structure so a previously-seen tree still lists in full
        // (a flaky mount must never look like an empty directory).
        if let Some(ref db) = self.db {
            let persisted = { db.lock().unwrap().get_dir_listing(rel_path) };
            if let Ok(rows) = persisted {
                if !rows.is_empty() {
                    for (name, is_dir) in rows {
                        if Self::is_hidden_entry(&name) {
                            continue;
                        }
                        let ft = if is_dir { FileType::Directory } else { FileType::RegularFile };
                        // Locally-cached files (already in `entries`) win — they're
                        // definitely present and may be newer than the snapshot.
                        entries.entry(name).or_insert(ft);
                    }
                    let mut result: Vec<(String, FileType)> = entries.into_iter().collect();
                    result.sort_by(|a, b| a.0.cmp(&b.0));
                    // Deliberately do NOT refresh the persisted snapshot or the
                    // in-memory TTL cache from a stale read — the moment the NAS
                    // returns we want a live listing, not this frozen one.
                    return DirListing::Complete(result);
                }
            }
        }

        // Genuinely unknown: NAS down, not cached, never seen before.
        DirListing::Unavailable
    }

    /// Look up a single entry in the persisted directory tree. Returns
    /// `Some(is_dir)` when the entry was recorded on a previous NAS listing.
    fn tree_entry(&self, parent_rel: &str, name: &str) -> Option<bool> {
        let db = self.db.as_ref()?;
        db.lock().unwrap().get_tree_entry(parent_rel, name).ok().flatten()
    }

    /// Synthesize a minimal attr for an entry we know from the tree metadata but
    /// cannot stat right now (NAS down, bytes not cached). Directories browse
    /// normally; files report size 0 — enough to appear in listings, though
    /// their bytes can't be opened until the NAS returns.
    fn make_synthetic_attr(&self, ino: INodeNo, is_dir: bool) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: if is_dir { FileType::Directory } else { FileType::RegularFile },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: if is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Invalidate the cached directory listing for a given directory.
    fn invalidate_dir_cache(&self, dir_rel: &str) {
        self.dir_list_cache.lock().unwrap().remove(dir_rel);
    }

    fn alloc_fh(&self) -> u64 {
        let mut fh = self.next_fh.lock().unwrap();
        let val = *fh;
        *fh += 1;
        val
    }

    /// Join parent relative path with a child name to produce a child relative path.
    fn join_rel(parent_rel: &str, name: &str) -> String {
        if parent_rel.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", parent_rel, name)
        }
    }
}

/// Threshold for logging slow FUSE operations.
const SLOW_OP_THRESHOLD: Duration = Duration::from_millis(1);

impl Filesystem for NfsCacheFS {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let t0 = Instant::now();
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);

        let (_resolved, meta) = match self.resolve_with_meta(&child_rel) {
            Some(r) => r,
            None => {
                // Couldn't confirm the entry live. If the NAS is simply down,
                // fall back to the last-known tree metadata so a previously-seen
                // path still resolves during an outage. When the NAS is up, a
                // miss is authoritative → ENOENT.
                if !self.nas.is_up() {
                    if let Some(is_dir) = self.tree_entry(&parent_rel, name_str) {
                        let ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
                        let attr = self.make_synthetic_attr(INodeNo(ino), is_dir);
                        reply.entry(&TTL, &attr, Generation(0));
                        return;
                    }
                }
                debug!("LOOKUP {} → ENOENT ({:.1}ms)", child_rel, t0.elapsed().as_secs_f64() * 1000.0);
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
        let attr = self.make_attr(INodeNo(ino), &meta);
        let elapsed = t0.elapsed();
        if elapsed > SLOW_OP_THRESHOLD {
            debug!("LOOKUP {} ({:.1}ms)", child_rel, elapsed.as_secs_f64() * 1000.0);
        }
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let t0 = Instant::now();
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        // For the root directory, we need to synthesize attrs if neither path exists
        if rel_path.is_empty() {
            // Try cache_dir first (local, fast), then NAS via the breaker.
            let meta = fs::metadata(&self.cache_dir)
                .ok()
                .or_else(|| self.nas.metadata(&self.nas_path));
            match meta {
                Some(m) => {
                    let attr = self.make_attr(ino, &m);
                    reply.attr(&TTL, &attr);
                }
                None => {
                    // Synthesize a minimal root attr
                    let now = SystemTime::now();
                    let attr = FileAttr {
                        ino: INodeNo(ROOT_INO),
                        size: 0,
                        blocks: 0,
                        atime: now,
                        mtime: now,
                        ctime: now,
                        crtime: now,
                        kind: FileType::Directory,
                        perm: 0o755,
                        nlink: 2,
                        uid: self.uid,
                        gid: self.gid,
                        rdev: 0,
                        blksize: 512,
                        flags: 0,
                    };
                    reply.attr(&TTL, &attr);
                }
            }
            return;
        }

        match self.resolve_with_meta(&rel_path) {
            Some((_path, m)) => {
                let attr = self.make_attr(ino, &m);
                let elapsed = t0.elapsed();
                if elapsed > SLOW_OP_THRESHOLD {
                    debug!("GETATTR {} ({:.1}ms)", rel_path, elapsed.as_secs_f64() * 1000.0);
                }
                reply.attr(&TTL, &attr);
            }
            None => {
                // During a NAS outage, serve a synthetic attr for an entry we know
                // from the tree metadata so `stat`/`ls -l` and directory walks keep
                // working offline. NAS up → the miss is real → ENOENT.
                if !self.nas.is_up() {
                    if let (Some(parent), Some(name)) =
                        (Self::parent_dir(&rel_path).or(Some("")), rel_path.rsplit('/').next())
                    {
                        if let Some(is_dir) = self.tree_entry(parent, name) {
                            let attr = self.make_synthetic_attr(ino, is_dir);
                            reply.attr(&TTL, &attr);
                            return;
                        }
                    }
                }
                reply.error(Errno::ENOENT)
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let t0 = Instant::now();
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let mut entries: Vec<(u64, FileType, String)> = Vec::new();

        // "." and ".."
        entries.push((ino.0, FileType::Directory, ".".to_string()));
        let parent_ino = if rel_path.is_empty() {
            ROOT_INO
        } else {
            // Find parent inode
            let parent_rel = if let Some(pos) = rel_path.rfind('/') {
                &rel_path[..pos]
            } else {
                ""
            };
            let inodes = self.inodes.lock().unwrap();
            inodes
                .path_to_ino
                .get(parent_rel)
                .copied()
                .unwrap_or(ROOT_INO)
        };
        entries.push((parent_ino, FileType::Directory, "..".to_string()));

        // Merged directory listing — lock inodes once for all children
        let children = match self.list_dir(&rel_path) {
            DirListing::Complete(c) => c,
            DirListing::Unavailable => {
                // NAS unreachable and no cached/known listing. Return EIO rather
                // than a truncated (empty) listing — a caller must be able to tell
                // "couldn't read this directory" apart from "directory is empty".
                warn!("READDIR {} → EIO (NAS unreachable, no cached listing)",
                    if rel_path.is_empty() { "/" } else { &rel_path });
                reply.error(Errno::EIO);
                return;
            }
        };
        {
            let mut inodes = self.inodes.lock().unwrap();
            for (name, ft) in children {
                let child_rel = Self::join_rel(&rel_path, &name);
                let child_ino = inodes.get_or_create(&child_rel);
                entries.push((child_ino, ft, name));
            }
        }

        let n_children = entries.len().saturating_sub(2); // exclude . and ..
        for (i, (ino, ft, name)) in entries.iter().enumerate().skip(offset as usize) {
            // reply.add returns true when buffer is full
            if reply.add(INodeNo(*ino), (i + 1) as u64, *ft, &name) {
                break;
            }
        }
        let elapsed = t0.elapsed();
        if elapsed > SLOW_OP_THRESHOLD {
            let cached = self.is_cached(&rel_path);
            debug!("READDIR {} ({} entries, cached={}, {:.1}ms)",
                if rel_path.is_empty() { "/" } else { &rel_path },
                n_children, cached, elapsed.as_secs_f64() * 1000.0);
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let t0 = Instant::now();
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let is_write = flags.0 & libc::O_WRONLY != 0 || flags.0 & libc::O_RDWR != 0;
        let cached = self.is_cached(&rel_path);

        // Fast path: file is in a cached directory — open directly from cache,
        // no resolve, no trigger_dir_cache, no NAS I/O
        if cached {
            let cache_path = self.cache_dir.join(&rel_path);
            let file = match OpenOptions::new()
                .read(true)
                .write(is_write)
                .open(&cache_path)
            {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to open cached {:?}: {}", cache_path, e);
                    reply.error(Errno::EIO);
                    return;
                }
            };

            let fh = self.alloc_fh();
            self.file_handles
                .lock()
                .unwrap()
                .insert(fh, OpenFileHandle { file, is_cached: true, dirty: false });

            let elapsed = t0.elapsed();
            if elapsed > SLOW_OP_THRESHOLD {
                debug!("OPEN {} (cached, {:.1}ms)", rel_path, elapsed.as_secs_f64() * 1000.0);
            }
            reply.opened(FileHandle(fh), FopenFlags::empty());
            return;
        }

        // Slow path: not cached — resolve from NAS, trigger background caching
        self.trigger_dir_cache(&rel_path);

        let resolved = match self.resolve(&rel_path) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // If writing and file is on NAS, copy to cache first
        let (open_path, is_cached) = if is_write && !resolved.starts_with(&self.cache_dir) {
            let cache_path = self.cache_dir.join(&rel_path);
            if let Some(parent) = cache_path.parent() {
                fs::create_dir_all(parent).ok();
            }
            if fs::copy(&resolved, &cache_path).is_ok() {
                info!("Copied to cache for write: {}", rel_path);
                self.pending_writes.lock().unwrap().insert(rel_path.clone());
                if let Some(ref db) = self.db {
                    let db = db.lock().unwrap();
                    db.add_pending_write(&rel_path).ok();
                }
                (cache_path, true)
            } else {
                warn!("Failed to copy to cache for write, using NAS directly: {}", rel_path);
                (resolved, false)
            }
        } else {
            let is_cached = resolved.starts_with(&self.cache_dir);
            (resolved, is_cached)
        };

        let file = match OpenOptions::new()
            .read(true)
            .write(is_write)
            .open(&open_path)
        {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to open {:?}: {}", open_path, e);
                reply.error(Errno::EIO);
                return;
            }
        };

        let fh = self.alloc_fh();
        self.file_handles
            .lock()
            .unwrap()
            .insert(fh, OpenFileHandle { file, is_cached, dirty: false });

        let elapsed = t0.elapsed();
        let source = if is_cached { "cache" } else { "NAS" };
        if elapsed > SLOW_OP_THRESHOLD {
            debug!("OPEN {} ({}, {:.1}ms)", rel_path, source, elapsed.as_secs_f64() * 1000.0);
        }
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let t0 = Instant::now();
        // Lock only long enough to clone the file handle, then release
        let file_result = {
            let handles = self.file_handles.lock().unwrap();
            match handles.get(&fh.0) {
                Some(h) => {
                    // Use pread to avoid needing &mut and allow concurrent reads
                    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&h.file);
                    Ok(fd)
                }
                None => Err(()),
            }
        };

        let fd = match file_result {
            Ok(fd) => fd,
            Err(()) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Read without holding the file_handles lock
        let mut buf = vec![0u8; size as usize];
        let n = unsafe {
            libc::pread(fd, buf.as_mut_ptr() as *mut libc::c_void, size as usize, offset as i64)
        };
        if n < 0 {
            error!("pread error: {}", std::io::Error::last_os_error());
            reply.error(Errno::EIO);
        } else {
            let elapsed = t0.elapsed();
            if elapsed > Duration::from_millis(10) {
                let rel_path = self.inodes.lock().unwrap().get_path(ino.0).map(|p| p.to_string());
                debug!("READ {} offset={} size={} ({:.1}ms)",
                    rel_path.as_deref().unwrap_or("?"), offset, n, elapsed.as_secs_f64() * 1000.0);
            }
            reply.data(&buf[..n as usize]);
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self.file_handles.lock().unwrap().remove(&fh.0);
        let was_dirty = handle.map(|h| h.dirty && h.is_cached).unwrap_or(false);

        // Fast path: file was only read, not modified — just close, no DB/stat work
        if !was_dirty {
            reply.ok();
            return;
        }

        // Slow path: file was modified — update DB and queue for NAS sync
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            inodes.get_path(ino.0).map(|p| p.to_string())
        };
        if let Some(rel) = rel_path {
            let cache_path = self.cache_dir.join(&rel);
            if let Ok(meta) = fs::metadata(&cache_path) {
                let mtime = meta.modified()
                    .unwrap_or(UNIX_EPOCH)
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                self.db_add(&rel, meta.len(), mtime);
            }

            let file_name = std::path::Path::new(&rel)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if !Self::is_temp_file(file_name) {
                info!("File modified, queuing for NAS sync: {}", rel);
                self.pending_writes.lock().unwrap().insert(rel.clone());
                if let Some(ref db) = self.db {
                    let db = db.lock().unwrap();
                    db.add_pending_write(&rel).ok();
                }
            }
        }

        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);
        info!("CREATE: {} (cache, pending NAS sync)", child_rel);
        let cache_path = self.cache_dir.join(&child_rel);

        // Create in cache only — NAS sync happens in background
        if let Some(p) = cache_path.parent() {
            fs::create_dir_all(p).ok();
        }

        if let Err(e) = File::create(&cache_path) {
            error!("Failed to create cache file {:?}: {}", cache_path, e);
            reply.error(Errno::EIO);
            return;
        }

        // Set permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            fs::set_permissions(&cache_path, perms).ok();
        }

        // Mark as pending NAS write (skip temp files)
        let file_name = std::path::Path::new(&child_rel)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if !Self::is_temp_file(file_name) {
            self.pending_writes.lock().unwrap().insert(child_rel.clone());
            if let Some(ref db) = self.db {
                let db = db.lock().unwrap();
                db.add_pending_write(&child_rel).ok();
            }
        }

        // Open the cache copy for subsequent read/write
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cache_path)
        {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to open created file: {}", e);
                reply.error(Errno::EIO);
                return;
            }
        };

        let meta = match fs::metadata(&cache_path) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
        let attr = self.make_attr(INodeNo(ino), &meta);
        let fh = self.alloc_fh();
        self.file_handles
            .lock()
            .unwrap()
            .insert(fh, OpenFileHandle { file, is_cached: true, dirty: false });

        self.invalidate_dir_cache(&parent_rel);
        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        // Write to cache only — NAS sync happens in background
        let mut handles = self.file_handles.lock().unwrap();
        let handle = match handles.get_mut(&fh.0) {
            Some(h) => h,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if let Err(e) = handle.file.seek(SeekFrom::Start(offset)) {
            error!("write seek error: {}", e);
            reply.error(Errno::EIO);
            return;
        }
        match handle.file.write(data) {
            Ok(n) => {
                handle.dirty = true;
                reply.written(n as u32);
            }
            Err(e) => {
                error!("write error: {}", e);
                reply.error(Errno::EIO);
            }
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);
        let nas_dir = self.nas_path.join(&child_rel);
        let cache_dir = self.cache_dir.join(&child_rel);

        // Create on NAS
        if let Err(e) = fs::create_dir_all(&nas_dir) {
            error!("mkdir NAS failed {:?}: {}", nas_dir, e);
            reply.error(Errno::EIO);
            return;
        }

        // Create in cache
        fs::create_dir_all(&cache_dir).ok();

        // Set permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            fs::set_permissions(&nas_dir, perms.clone()).ok();
            fs::set_permissions(&cache_dir, perms).ok();
        }

        let meta = match fs::metadata(&nas_dir) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
        let attr = self.make_attr(INodeNo(ino), &meta);
        self.invalidate_dir_cache(&parent_rel);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);
        info!("DELETE: {}", child_rel);

        // Delete from NAS
        let nas_path = self.nas_path.join(&child_rel);
        if nas_path.exists() {
            if let Err(e) = fs::remove_file(&nas_path) {
                error!("DELETE NAS failed {:?}: {}", nas_path, e);
                reply.error(Errno::EIO);
                return;
            }
            info!("  NAS: removed");
        }

        // Delete from cache
        let cache_path = self.cache_dir.join(&child_rel);
        if cache_path.exists() {
            fs::remove_file(&cache_path).ok();
            info!("  Cache: removed");
        }

        self.db_remove(&child_rel);
        // Clear from pending writes
        self.pending_writes.lock().unwrap().remove(&child_rel);
        if let Some(ref db) = self.db {
            let db = db.lock().unwrap();
            db.remove_pending_write(&child_rel).ok();
        }
        info!("  DB: removed");
        self.inodes.lock().unwrap().remove_path(&child_rel);
        self.invalidate_dir_cache(&parent_rel);
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);
        info!("RMDIR: {}", child_rel);

        // Check that merged directory is empty. If we can't get a trustworthy
        // listing (NAS unreachable), refuse rather than risk removing a directory
        // whose contents we can't see.
        let children = match self.list_dir(&child_rel) {
            DirListing::Complete(c) => c,
            DirListing::Unavailable => {
                reply.error(Errno::EIO);
                return;
            }
        };
        if !children.is_empty() {
            reply.error(Errno::ENOTEMPTY);
            return;
        }

        // Remove from NAS
        let nas_dir = self.nas_path.join(&child_rel);
        if nas_dir.exists() {
            if let Err(e) = fs::remove_dir(&nas_dir) {
                error!("RMDIR NAS failed {:?}: {}", nas_dir, e);
                reply.error(Errno::EIO);
                return;
            }
            info!("  NAS: removed");
        }

        // Remove from cache
        let cache_dir = self.cache_dir.join(&child_rel);
        if cache_dir.exists() {
            fs::remove_dir(&cache_dir).ok();
            info!("  Cache: removed");
        }

        // Remove from in-memory tracking
        self.cached_dirs.lock().unwrap().remove(&child_rel);
        self.empty_dirs.lock().unwrap().remove(&child_rel);
        if let Some(ref db) = self.db {
            let db = db.lock().unwrap();
            let _ = db.remove_dir(&child_rel);
        }

        self.inodes.lock().unwrap().remove_path(&child_rel);
        self.invalidate_dir_cache(&parent_rel);
        self.invalidate_dir_cache(&child_rel);
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };
        let newname_str = match newname.to_str() {
            Some(n) => n,
            None => {
                reply.error(Errno::EIO);
                return;
            }
        };

        let (parent_rel, newparent_rel) = {
            let inodes = self.inodes.lock().unwrap();
            let p = match inodes.get_path(parent.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            let np = match inodes.get_path(newparent.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            (p, np)
        };

        let old_rel = Self::join_rel(&parent_rel, name_str);
        let new_rel = Self::join_rel(&newparent_rel, newname_str);
        info!("RENAME: {} -> {}", old_rel, new_rel);

        // Rename on NAS
        let old_nas = self.nas_path.join(&old_rel);
        let new_nas = self.nas_path.join(&new_rel);
        if old_nas.exists() {
            if let Some(p) = new_nas.parent() {
                fs::create_dir_all(p).ok();
            }
            if let Err(e) = fs::rename(&old_nas, &new_nas) {
                error!("RENAME NAS failed: {}", e);
                reply.error(Errno::EIO);
                return;
            }
            info!("  NAS: renamed");
        }

        // Rename in cache
        let old_cache = self.cache_dir.join(&old_rel);
        let new_cache = self.cache_dir.join(&new_rel);
        if old_cache.exists() {
            if let Some(p) = new_cache.parent() {
                fs::create_dir_all(p).ok();
            }
            fs::rename(&old_cache, &new_cache).ok();
            info!("  Cache: renamed");
        }

        self.db_rename(&old_rel, &new_rel);

        // Update in-memory cache tracking
        {
            let mut dirs = self.cached_dirs.lock().unwrap();
            if dirs.remove(&old_rel) {
                dirs.insert(new_rel.clone());
            }
        }
        {
            let mut dirs = self.empty_dirs.lock().unwrap();
            if dirs.remove(&old_rel) {
                dirs.insert(new_rel.clone());
            }
        }
        // Update pending writes that start with old path
        {
            let mut pending = self.pending_writes.lock().unwrap();
            let old_prefix = format!("{}/", old_rel);
            let to_rename: Vec<String> = pending.iter()
                .filter(|p| p.starts_with(&old_prefix) || **p == old_rel)
                .cloned()
                .collect();
            for old_path in &to_rename {
                pending.remove(old_path);
                let new_path = format!("{}{}", new_rel, &old_path[old_rel.len()..]);
                pending.insert(new_path);
            }
        }
        // Update DB directory entry
        if let Some(ref db) = self.db {
            let db = db.lock().unwrap();
            if db.is_dir_cached(&old_rel).unwrap_or(false) {
                let _ = db.remove_dir(&old_rel);
                let _ = db.touch_dir(&new_rel, 0);
            }
        }

        info!("  DB: updated");
        self.inodes.lock().unwrap().rename(&old_rel, &new_rel);
        self.invalidate_dir_cache(&parent_rel);
        if newparent_rel != parent_rel {
            self.invalidate_dir_cache(&newparent_rel);
        }
        reply.ok();
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let resolved = match self.resolve(&rel_path) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Handle truncate
        if let Some(new_size) = size {
            if let Ok(f) = OpenOptions::new().write(true).open(&resolved) {
                f.set_len(new_size).ok();
            }
            // Also truncate on NAS if resolved was cache
            let nas_path = self.nas_path.join(&rel_path);
            if nas_path.exists() && nas_path != resolved {
                if let Ok(f) = OpenOptions::new().write(true).open(&nas_path) {
                    f.set_len(new_size).ok();
                }
            }
        }

        // Handle permissions
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(m);
            fs::set_permissions(&resolved, perms).ok();
        }

        // Return updated attrs
        match fs::metadata(&resolved) {
            Ok(m) => {
                let attr = self.make_attr(ino, &m);
                reply.attr(&TTL, &attr);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::from_i32(libc::ENOATTR));
                    return;
                }
            }
        };

        let name_str = name.to_str().unwrap_or("");

        // Determine color: Orange = pending write, Green = cached (dir or file)
        // is_cached checks both the dir itself and its parent dir in cached_dirs set (no I/O).
        // is_file_cached does a stat syscall — only use it as a fallback for files whose
        // parent dir isn't fully cached (e.g. individually cached via write-back).
        let color = if self.is_pending_write(&rel_path) {
            Some(FINDER_COLOR_ORANGE)
        } else if self.is_cached(&rel_path) {
            Some(FINDER_COLOR_GREEN)
        } else if self.is_file_cached(&rel_path) {
            Some(FINDER_COLOR_GREEN)
        } else {
            None
        };

        if let Some(color_byte) = color {
            if name_str == FINDER_INFO_XATTR {
                let info = make_finder_info(color_byte);
                if size == 0 {
                    reply.size(FINDER_INFO_LEN as u32);
                } else if size >= FINDER_INFO_LEN as u32 {
                    reply.data(&info);
                } else {
                    reply.error(Errno::ERANGE);
                }
                return;
            }
        }

        reply.error(Errno::from_i32(libc::ENOATTR));
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino.0) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        if self.is_pending_write(&rel_path) || self.is_cached(&rel_path) || self.is_file_cached(&rel_path)  {
            // Only report FinderInfo xattr
            let mut names = Vec::from(FINDER_INFO_XATTR.as_bytes());
            names.push(0);
            if size == 0 {
                reply.size(names.len() as u32);
            } else if size >= names.len() as u32 {
                reply.data(&names);
            } else {
                reply.error(Errno::ERANGE);
            }
        } else if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn setxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        // Silently accept — prevents Finder "wants to change tag" prompts
        reply.ok();
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Use local cache dir for statfs — never block on NAS.
        // Lightroom calls this frequently to check disk space.
        let stv = unsafe {
            let mut buf: libc::statfs = std::mem::zeroed();
            let c_path = std::ffi::CString::new(
                self.cache_dir.as_os_str().as_encoded_bytes()
            ).unwrap_or_default();
            if libc::statfs(c_path.as_ptr(), &mut buf) == 0 {
                buf
            } else {
                reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
                return;
            }
        };
        reply.statfs(
            stv.f_blocks as u64,
            stv.f_bfree as u64,
            stv.f_bavail as u64,
            stv.f_files as u64,
            stv.f_ffree as u64,
            stv.f_bsize as u32,
            255,
            stv.f_bsize as u32,
        );
    }

}

/// Mount the NfsCacheFS at the given mount point.
/// If the mount point is already a FUSE mount, unmount it first.
/// Spawn a background thread that keeps the persistent directory-tree index
/// (`dir_tree`) complete by periodically crawling the whole NAS structure.
///
/// This is metadata only — it lists directories, it never downloads file bytes.
/// The payoff: the mount can present the entire tree at any depth, served from
/// the index during NAS outages, and file contents are fetched lazily only when
/// a file is actually opened.
fn spawn_metadata_crawler(
    nas_path: PathBuf,
    db: Arc<Mutex<CacheDB>>,
    nas: Arc<NasHealth>,
    interval: Duration,
) {
    let _ = thread::Builder::new()
        .name("nas-crawler".into())
        .spawn(move || loop {
            if nas.is_up() {
                let t0 = Instant::now();
                let n = crawl_tree(&nas_path, &nas, &db);
                if n > 0 {
                    info!(
                        "Metadata crawl indexed {} directories in {:.1}s",
                        n,
                        t0.elapsed().as_secs_f64()
                    );
                }
                thread::sleep(interval);
            } else {
                // NAS unreachable — check back soon rather than waiting a full cycle.
                thread::sleep(Duration::from_secs(30));
            }
        });
}

/// Walk the entire NAS tree breadth-first (each read bounded by the breaker) and
/// persist every directory's listing to `dir_tree`. Returns the number of
/// directories indexed. A directory whose read fails right now is skipped, not
/// emptied — its previously-indexed listing is left intact, and the crawl stops
/// descending the moment the NAS goes down, so a hung mount never wipes the index.
fn crawl_tree(nas_root: &Path, nas: &Arc<NasHealth>, db: &Arc<Mutex<CacheDB>>) -> usize {
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    queue.push_back(String::new()); // start at the root
    let mut count = 0usize;

    while let Some(rel) = queue.pop_front() {
        if !nas.is_up() {
            break; // NAS dropped mid-crawl; resume next cycle, keep what we have
        }
        let dir = if rel.is_empty() { nas_root.to_path_buf() } else { nas_root.join(&rel) };
        let names = match nas.run_with_timeout(NAS_READDIR_TIMEOUT, move || read_dir_names(&dir)) {
            Some(n) => n,
            None => continue, // unreadable right now — leave existing metadata intact
        };

        let mut rows: Vec<(String, bool)> = Vec::with_capacity(names.len());
        for (name, is_dir) in &names {
            if NfsCacheFS::is_hidden_entry(name) {
                continue;
            }
            rows.push((name.clone(), *is_dir));
            if *is_dir {
                queue.push_back(NfsCacheFS::join_rel(&rel, name));
            }
        }

        if let Ok(db) = db.lock() {
            let _ = db.replace_dir_listing(&rel, &rows);
        }
        count += 1;
        // Be gentle on a flaky NAS between directory reads.
        thread::sleep(Duration::from_millis(20));
    }
    count
}

pub fn mount(
    nas_path: PathBuf,
    cache_dir: PathBuf,
    mount_point: &Path,
    db_path: &Path,
    max_cache_bytes: u64,
) {
    // Try to unmount if already mounted (force to handle busy mounts)
    let _ = std::process::Command::new("umount")
        .arg("-f")
        .arg(mount_point)
        .output();

    fs::create_dir_all(mount_point).ok();
    fs::create_dir_all(&cache_dir).ok();

    let db = CacheDB::open(db_path).ok();

    // Spawn background cache worker with its own DB connection
    let worker = {
        let worker_db = CacheDB::open(db_path).expect("Failed to open worker DB");
        CacheWorker::spawn(
            nas_path.clone(),
            cache_dir.clone(),
            Arc::new(Mutex::new(worker_db)),
            max_cache_bytes,
        )
    };

    // Spawn background write flusher (syncs local writes to NAS every 5 seconds)
    let flush_worker = {
        let flush_db = CacheDB::open(db_path).expect("Failed to open flush DB");
        WriteFlushWorker::spawn(
            nas_path.clone(),
            cache_dir.clone(),
            Arc::new(Mutex::new(flush_db)),
            std::time::Duration::from_secs(5),
            max_cache_bytes,
        )
    };

    let filesystem = NfsCacheFS::new(nas_path.clone(), cache_dir, db, Some(worker), Some(flush_worker));

    // Background crawler: keep the full directory-tree index current so the mount
    // can list every folder at any depth (served from the index during outages)
    // while file contents stay download-on-open. Shares the FS breaker.
    {
        let crawl_db = CacheDB::open(db_path).expect("Failed to open crawler DB");
        spawn_metadata_crawler(
            nas_path,
            Arc::new(Mutex::new(crawl_db)),
            filesystem.nas_handle(),
            Duration::from_secs(600),
        );
    }

    let mut config = Config::default();
    let vol_name = mount_point
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nfscache".to_string());
    config.mount_options = vec![
        MountOption::FSName("nfscache".to_string()),
        MountOption::CUSTOM(format!("volname={}", vol_name)),
    ];
    if let Err(e) = fuser::mount2(filesystem, mount_point, &config) {
        eprintln!("Failed to mount at {}: {}", mount_point.display(), e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_inode_map_root() {
        let map = InodeMap::new();
        assert_eq!(map.get_path(ROOT_INO), Some(""));
        assert_eq!(map.path_to_ino.get(""), Some(&ROOT_INO));
    }

    #[test]
    fn test_inode_map_get_or_create() {
        let mut map = InodeMap::new();
        let ino1 = map.get_or_create("March 2026/IMG_001.jpg");
        let ino2 = map.get_or_create("March 2026/IMG_001.jpg");
        assert_eq!(ino1, ino2);
        assert!(ino1 >= 2); // root is 1

        let ino3 = map.get_or_create("March 2026/IMG_002.jpg");
        assert_ne!(ino1, ino3);
    }

    #[test]
    fn test_inode_map_remove() {
        let mut map = InodeMap::new();
        let ino = map.get_or_create("test.jpg");
        map.remove_path("test.jpg");
        assert!(map.get_path(ino).is_none());
        assert!(map.path_to_ino.get("test.jpg").is_none());
    }

    #[test]
    fn test_inode_map_rename() {
        let mut map = InodeMap::new();
        let ino = map.get_or_create("old.jpg");
        map.rename("old.jpg", "new.jpg");
        assert_eq!(map.get_path(ino), Some("new.jpg"));
        assert!(map.path_to_ino.get("old.jpg").is_none());
        assert_eq!(map.path_to_ino.get("new.jpg"), Some(&ino));
    }

    #[test]
    fn test_resolve_prefers_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        // File exists in both NAS and cache
        fs::write(nas.join("photo.jpg"), b"nas-version").unwrap();
        fs::write(cache.join("photo.jpg"), b"cache-version").unwrap();

        let fs = NfsCacheFS::new(nas.clone(), cache.clone(), None, None, None);
        let resolved = fs.resolve("photo.jpg").unwrap();
        assert_eq!(resolved, cache.join("photo.jpg"));
    }

    #[test]
    fn test_resolve_falls_back_to_nas() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        // File only on NAS
        fs::write(nas.join("photo.jpg"), b"nas-only").unwrap();

        let fs = NfsCacheFS::new(nas.clone(), cache.clone(), None, None, None);
        let resolved = fs.resolve("photo.jpg").unwrap();
        assert_eq!(resolved, nas.join("photo.jpg"));
    }

    #[test]
    fn test_resolve_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let fs = NfsCacheFS::new(nas, cache, None, None, None);
        assert!(fs.resolve("nonexistent.jpg").is_none());
    }

    #[test]
    fn test_list_dir_merges() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        // NAS has file A, cache has file B
        fs::write(nas.join("a.jpg"), b"aaa").unwrap();
        fs::write(cache.join("b.jpg"), b"bbb").unwrap();

        let fs = NfsCacheFS::new(nas, cache, None, None, None);
        let entries = fs.list_dir("").into_entries();
        let names: HashSet<String> = entries.into_iter().map(|(n, _)| n).collect();
        assert!(names.contains("a.jpg"));
        assert!(names.contains("b.jpg"));
    }

    #[test]
    fn test_list_dir_deduplicates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        // Same file in both
        fs::write(nas.join("photo.jpg"), b"nas").unwrap();
        fs::write(cache.join("photo.jpg"), b"cache").unwrap();

        let fs = NfsCacheFS::new(nas, cache, None, None, None);
        let entries = fs.list_dir("").into_entries();
        let names: Vec<String> = entries.into_iter().map(|(n, _)| n).collect();
        assert_eq!(names.iter().filter(|n| *n == "photo.jpg").count(), 1);
    }

    #[test]
    fn test_join_rel() {
        assert_eq!(NfsCacheFS::join_rel("", "file.jpg"), "file.jpg");
        assert_eq!(
            NfsCacheFS::join_rel("March 2026", "IMG_001.jpg"),
            "March 2026/IMG_001.jpg"
        );
    }

    // --- is_hidden_entry tests ---

    #[test]
    fn test_is_hidden_entry_synology() {
        assert!(NfsCacheFS::is_hidden_entry("@eaDir"));
    }

    #[test]
    fn test_is_hidden_entry_resource_fork() {
        assert!(NfsCacheFS::is_hidden_entry("._IMG_001.jpg"));
        assert!(NfsCacheFS::is_hidden_entry("._photo.heic"));
    }

    #[test]
    fn test_is_hidden_entry_syno_files() {
        assert!(NfsCacheFS::is_hidden_entry("@SynoResource"));
        assert!(NfsCacheFS::is_hidden_entry("file@SynoExt"));
    }

    #[test]
    fn test_is_hidden_entry_normal_files() {
        assert!(!NfsCacheFS::is_hidden_entry("IMG_001.jpg"));
        assert!(!NfsCacheFS::is_hidden_entry("March 2026"));
        assert!(!NfsCacheFS::is_hidden_entry(".hidden_dir"));
    }

    #[test]
    fn test_is_hidden_entry_temp_files() {
        assert!(NfsCacheFS::is_hidden_entry("DSC_0913.tmp"));
        assert!(NfsCacheFS::is_hidden_entry("photo.jpg.tmp"));
        assert!(!NfsCacheFS::is_hidden_entry("DSC_0913.dng"));
    }

    // --- is_file_cached tests ---

    #[test]
    fn test_is_file_cached_with_cached_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(cache.join("March 2026")).unwrap();
        fs::write(cache.join("March 2026/IMG_001.jpg"), b"data").unwrap();

        let fsys = NfsCacheFS::new(nas, cache, None, None, None);
        assert!(fsys.is_file_cached("March 2026/IMG_001.jpg"));
    }

    #[test]
    fn test_is_file_cached_without_cached_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, None, None, None);
        assert!(!fsys.is_file_cached("March 2026/IMG_001.jpg"));
    }

    #[test]
    fn test_is_file_cached_rejects_root_and_bare_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, None, None, None);
        assert!(!fsys.is_file_cached(""), "empty path should not be cached");
        assert!(!fsys.is_file_cached("toplevel.jpg"), "bare filename without dir should not be cached");
    }

    // --- is_cached tests ---

    #[test]
    fn test_is_cached_with_cached_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let db = crate::cache_db::CacheDB::open(Path::new(":memory:")).unwrap();
        db.touch_dir("March 2026", 5000).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, Some(db), None, None);
        assert!(fsys.is_cached("March 2026"));
    }

    #[test]
    fn test_is_cached_file_in_cached_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let db = crate::cache_db::CacheDB::open(Path::new(":memory:")).unwrap();
        db.touch_dir("March 2026", 5000).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, Some(db), None, None);
        assert!(fsys.is_cached("March 2026/IMG_001.jpg"));
    }

    #[test]
    fn test_is_cached_uncached_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, None, None, None);
        assert!(!fsys.is_cached("Uncached Dir"));
        assert!(!fsys.is_cached("Uncached Dir/photo.jpg"));
        assert!(!fsys.is_cached(""), "root should not be cached");
    }

    // --- parent_dir tests ---

    #[test]
    fn test_parent_dir_with_file_in_dir() {
        assert_eq!(NfsCacheFS::parent_dir("March 2026/IMG_001.jpg"), Some("March 2026"));
    }

    #[test]
    fn test_parent_dir_bare_name() {
        assert_eq!(NfsCacheFS::parent_dir("toplevel"), None);
    }

    #[test]
    fn test_parent_dir_empty() {
        assert_eq!(NfsCacheFS::parent_dir(""), None);
    }

    #[test]
    fn test_parent_dir_nested() {
        // Returns the immediate parent directory
        assert_eq!(NfsCacheFS::parent_dir("a/b/c"), Some("a/b"));
    }

    // --- make_finder_info tests ---

    #[test]
    fn test_make_finder_info_green() {
        let info = make_finder_info(FINDER_COLOR_GREEN);
        assert_eq!(info.len(), FINDER_INFO_LEN);
        assert_eq!(info[9], FINDER_COLOR_GREEN);
        // All other bytes should be zero
        for (i, &b) in info.iter().enumerate() {
            if i != 9 {
                assert_eq!(b, 0, "byte {} should be 0", i);
            }
        }
    }

    #[test]
    fn test_make_finder_info_yellow() {
        let info = make_finder_info(5 << 1); // yellow
        assert_eq!(info[9], 0x0A);
    }

    #[test]
    fn test_make_finder_info_orange() {
        let info = make_finder_info(FINDER_COLOR_ORANGE);
        assert_eq!(info[9], FINDER_COLOR_ORANGE);
        assert_eq!(info[9], 0x0E);
    }

    #[test]
    fn test_make_finder_info_is_correct_size() {
        let info = make_finder_info(0);
        assert_eq!(info.len(), 32);
    }

    // --- list_dir filters hidden entries ---

    #[test]
    fn test_list_dir_hides_synology_metadata() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        fs::write(nas.join("photo.jpg"), b"data").unwrap();
        fs::create_dir_all(nas.join("@eaDir")).unwrap();
        fs::write(nas.join("._photo.jpg"), b"resource fork").unwrap();

        let fsys = NfsCacheFS::new(nas, cache, None, None, None);
        let entries = fsys.list_dir("").into_entries();
        let names: Vec<String> = entries.into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"photo.jpg".to_string()));
        assert!(!names.contains(&"@eaDir".to_string()));
        assert!(!names.contains(&"._photo.jpg".to_string()));
    }

    // --- directory-listing reliability (tree metadata cache) ---

    #[test]
    fn test_list_dir_unavailable_when_nas_down_and_unknown() {
        // NAS unreachable, directory never seen, not cached → must report
        // Unavailable (→ EIO) rather than an empty listing. This is the bug that
        // let photosync treat already-imported photos as "new".
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let nas = tmp.path().join("nonexistent-nas"); // never created → breaker starts down
        let db = CacheDB::open(Path::new(":memory:")).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, Some(db), None, None);
        assert!(!fsys.nas.is_up(), "breaker should start down for a missing NAS");
        assert!(matches!(fsys.list_dir("2025"), DirListing::Unavailable));
    }

    #[test]
    fn test_list_dir_persists_structure_from_live_nas() {
        // A successful live listing records the directory structure so it can be
        // served later without the NAS.
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(nas.join("2025/August")).unwrap();
        fs::write(nas.join("2025/photo.jpg"), b"x").unwrap();
        fs::create_dir_all(&cache).unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, Some(db), None, None);
        assert!(fsys.nas.is_up());
        let _ = fsys.list_dir("2025").into_entries();

        let mut listing = {
            let db = fsys.db.as_ref().unwrap().lock().unwrap();
            db.get_dir_listing("2025").unwrap()
        };
        listing.sort();
        assert_eq!(listing, vec![
            ("August".to_string(), true),
            ("photo.jpg".to_string(), false),
        ]);
    }

    #[test]
    fn test_list_dir_serves_metadata_during_outage() {
        // With structure already recorded, a NAS outage still yields a complete
        // listing (from the persisted tree), not a truncated one.
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let nas = tmp.path().join("nonexistent-nas"); // breaker down
        let db = CacheDB::open(Path::new(":memory:")).unwrap();
        db.replace_dir_listing("2025", &[
            ("August".to_string(), true),
            ("DSC_1.jpg".to_string(), false),
        ]).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, Some(db), None, None);
        assert!(!fsys.nas.is_up());
        let entries = match fsys.list_dir("2025") {
            DirListing::Complete(e) => e,
            DirListing::Unavailable => panic!("expected a complete listing from tree metadata"),
        };
        let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
        assert!(names.contains(&"August".to_string()));
        assert!(names.contains(&"DSC_1.jpg".to_string()));
    }

    #[test]
    fn test_list_dir_serves_metadata_when_live_read_fails_but_breaker_up() {
        // Regression: a soft-mount EIO returns fast, so the breaker stays UP while
        // the directory read still fails. list_dir must fall back to the index —
        // not present an empty/truncated listing — or photosync re-imports.
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap(); // root exists → breaker starts UP
        fs::create_dir_all(&cache).unwrap();
        let db = CacheDB::open(Path::new(":memory:")).unwrap();
        db.replace_dir_listing("2026/May", &[
            ("DSC_0457.dng".to_string(), false),
            ("DSC_0458.dng".to_string(), false),
        ]).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, Some(db), None, None);
        assert!(fsys.nas.is_up(), "breaker is up because the NAS root exists");
        // nas/2026/May does NOT exist on disk → the live read fails (NotFound),
        // fast, without tripping the breaker — the exact hole that leaked before.
        let entries = match fsys.list_dir("2026/May") {
            DirListing::Complete(e) => e,
            DirListing::Unavailable => panic!("must serve the index, not fail"),
        };
        let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
        assert!(names.contains(&"DSC_0457.dng".to_string()), "got {:?}", names);
        assert!(names.contains(&"DSC_0458.dng".to_string()), "got {:?}", names);
    }

    #[test]
    fn test_lookup_synthetic_attr_offline() {
        // tree_entry + make_synthetic_attr: a known entry resolves offline.
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let nas = tmp.path().join("nonexistent-nas");
        let db = CacheDB::open(Path::new(":memory:")).unwrap();
        db.replace_dir_listing("", &[("2025".to_string(), true)]).unwrap();

        let fsys = NfsCacheFS::new(nas, cache, Some(db), None, None);
        assert_eq!(fsys.tree_entry("", "2025"), Some(true));
        assert_eq!(fsys.tree_entry("", "missing"), None);
        let attr = fsys.make_synthetic_attr(INodeNo(42), true);
        assert_eq!(attr.kind, FileType::Directory);
        assert_eq!(attr.size, 0);
    }

    #[test]
    fn test_crawl_tree_indexes_every_level() {
        // The crawler records the whole tree — not just the top level — so the
        // mount can list any folder at any depth without touching the NAS.
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        fs::create_dir_all(nas.join("2025/August")).unwrap();
        fs::write(nas.join("2025/August/DSC_1.jpg"), b"x").unwrap();
        fs::write(nas.join("2025/photo.jpg"), b"y").unwrap();
        fs::create_dir_all(nas.join("2024/January")).unwrap();
        fs::write(nas.join("2024/January/old.jpg"), b"z").unwrap();
        // Synology metadata must be filtered out of the index.
        fs::create_dir_all(nas.join("2025/@eaDir")).unwrap();

        let nas_health = Arc::new(NasHealth::spawn(
            nas.clone(),
            Duration::from_secs(3),
            Duration::from_secs(60),
        ));
        assert!(nas_health.is_up());
        let db = Arc::new(Mutex::new(CacheDB::open(Path::new(":memory:")).unwrap()));

        let n = crawl_tree(&nas, &nas_health, &db);
        assert!(n >= 5, "expected root + 2024 + 2024/January + 2025 + 2025/August, got {}", n);

        let db = db.lock().unwrap();
        let mut root = db.get_dir_listing("").unwrap();
        root.sort();
        assert_eq!(root, vec![("2024".to_string(), true), ("2025".to_string(), true)]);
        // Deep entries are indexed too.
        assert_eq!(db.get_dir_listing("2025/August").unwrap(), vec![("DSC_1.jpg".to_string(), false)]);
        assert_eq!(db.get_tree_entry("2024", "January").unwrap(), Some(true));
        // Hidden metadata is not indexed.
        assert_eq!(db.get_tree_entry("2025", "@eaDir").unwrap(), None);
    }
}
