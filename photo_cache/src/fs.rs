// src/fs.rs — FUSE filesystem presenting a unified view of NAS + local cache
use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    RenameFlags, Request, TimeOrNow, WriteFlags,
};
use crate::cache_db::CacheDB;
use crate::sync::CacheWorker;
use crate::sync::WriteFlushWorker;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(30);
const ROOT_INO: u64 = 1;
const FINDER_TAGS_XATTR: &str = "com.apple.metadata:_kMDItemUserTags";
const FINDER_INFO_XATTR: &str = "com.apple.FinderInfo";
const FINDER_INFO_LEN: usize = 32;

// FinderInfo color label values for byte 9: color << 1 (bits 3:1)
// Verified mapping: 1=gray, 2=green, 3=purple, 4=blue, 5=yellow, 6=red, 7=orange
const FINDER_COLOR_GREEN: u8 = 2 << 1;  // 0x04
const FINDER_COLOR_YELLOW: u8 = 5 << 1; // 0x0A
const FINDER_COLOR_ORANGE: u8 = 7 << 1; // 0x0E

fn make_finder_info(color_byte: u8) -> [u8; FINDER_INFO_LEN] {
    let mut info = [0u8; FINDER_INFO_LEN];
    info[9] = color_byte;
    info
}


// Binary plist encoding of ["Green\n2"] — file cached locally
// Tag color indices: 0=none, 1=gray, 2=green, 3=purple, 4=blue, 5=yellow, 6=red, 7=orange
const GREEN_TAG_PLIST: &[u8] = &[
    98, 112, 108, 105, 115, 116, 48, 48, 161, 1, 87, 71, 114, 101, 101, 110, 10, 50,
    8, 10, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 18,
];

// Binary plist encoding of ["Orange\n7"] — pending NAS write tag
const ORANGE_TAG_PLIST: &[u8] = &[
    98, 112, 108, 105, 115, 116, 48, 48, 161, 1, 88, 79, 114, 97, 110, 103, 101, 10,
    55, 8, 10, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 19,
];

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
}

pub struct PhotoCacheFS {
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
    /// In-memory set of files pending NAS write.
    pending_writes: Mutex<HashSet<String>>,
    uid: u32,
    gid: u32,
}

impl PhotoCacheFS {
    pub fn new(
        nas_path: PathBuf,
        cache_dir: PathBuf,
        db: Option<CacheDB>,
        cache_worker: Option<CacheWorker>,
        flush_worker: Option<WriteFlushWorker>,
    ) -> Self {
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };

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

        PhotoCacheFS {
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
            uid,
            gid,
        }
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
                // Touch the dir to update LRU
                if let Some(ref db) = self.db {
                    let db = db.lock().unwrap();
                    let _ = db.touch_dir_access(dir);
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
            if let Err(e) = db.add(rel_path, size, mtime) {
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
                let _ = db.add(new_path, entry.size, entry.mtime);
            }
        }
    }

    /// Resolve a relative path to the actual filesystem path.
    /// Checks cache first, then NAS.
    fn resolve(&self, rel_path: &str) -> Option<PathBuf> {
        if rel_path.is_empty() {
            // Root directory: prefer cache_dir if it exists, else nas_path
            if self.cache_dir.exists() {
                return Some(self.cache_dir.clone());
            }
            if self.nas_path.exists() {
                return Some(self.nas_path.clone());
            }
            return None;
        }
        let cached = self.cache_dir.join(rel_path);
        if cached.exists() {
            return Some(cached);
        }
        let nas = self.nas_path.join(rel_path);
        if nas.exists() {
            return Some(nas);
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
    }

    fn list_dir(&self, rel_path: &str) -> Vec<(String, FileType)> {
        let mut entries: HashMap<String, FileType> = HashMap::new();

        // Read from NAS
        let nas_dir = if rel_path.is_empty() {
            self.nas_path.clone()
        } else {
            self.nas_path.join(rel_path)
        };
        if let Ok(read_dir) = fs::read_dir(&nas_dir) {
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

        // Read from cache (overrides/merges with NAS)
        let cache_dir = if rel_path.is_empty() {
            self.cache_dir.clone()
        } else {
            self.cache_dir.join(rel_path)
        };
        if let Ok(read_dir) = fs::read_dir(&cache_dir) {
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

        let mut result: Vec<(String, FileType)> = entries.into_iter().collect();
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
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

impl Filesystem for PhotoCacheFS {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
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

        let resolved = match self.resolve(&child_rel) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let meta = match fs::metadata(&resolved) {
            Ok(m) => m,
            Err(_) => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
        let attr = self.make_attr(INodeNo(ino), &meta);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
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
            // Try cache_dir first, then nas_path
            let meta = fs::metadata(&self.cache_dir)
                .or_else(|_| fs::metadata(&self.nas_path));
            match meta {
                Ok(m) => {
                    let attr = self.make_attr(ino, &m);
                    reply.attr(&TTL, &attr);
                }
                Err(_) => {
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

        let resolved = match self.resolve(&rel_path) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match fs::metadata(&resolved) {
            Ok(m) => {
                let attr = self.make_attr(ino, &m);
                reply.attr(&TTL, &attr);
            }
            Err(_) => reply.error(Errno::ENOENT),
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
        let children = self.list_dir(&rel_path);
        {
            let mut inodes = self.inodes.lock().unwrap();
            for (name, ft) in children {
                let child_rel = Self::join_rel(&rel_path, &name);
                let child_ino = inodes.get_or_create(&child_rel);
                entries.push((child_ino, ft, name));
            }
        }

        for (i, (ino, ft, name)) in entries.iter().enumerate().skip(offset as usize) {
            // reply.add returns true when buffer is full
            if reply.add(INodeNo(*ino), (i + 1) as u64, *ft, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
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

        // Trigger background caching of this file's directory
        self.trigger_dir_cache(&rel_path);

        let is_write = flags.0 & libc::O_WRONLY != 0 || flags.0 & libc::O_RDWR != 0;

        let resolved = match self.resolve(&rel_path) {
            Some(p) => p,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // If writing and file is on NAS (not cached), copy to cache first
        // so writes go to local disk and get flushed to NAS later
        let (open_path, is_cached) = if is_write && !resolved.starts_with(&self.cache_dir) {
            let cache_path = self.cache_dir.join(&rel_path);
            if let Some(parent) = cache_path.parent() {
                fs::create_dir_all(parent).ok();
            }
            if fs::copy(&resolved, &cache_path).is_ok() {
                info!("Copied to cache for write: {}", rel_path);
                // Mark as pending NAS write
                self.pending_writes.lock().unwrap().insert(rel_path.clone());
                if let Some(ref db) = self.db {
                    let db = db.lock().unwrap();
                    db.add_pending_write(&rel_path).ok();
                }
                (cache_path, true)
            } else {
                // Fallback: write directly to NAS
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
            .insert(fh, OpenFileHandle { file, is_cached });

        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut handles = self.file_handles.lock().unwrap();
        let handle = match handles.get_mut(&fh.0) {
            Some(h) => h,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let mut buf = vec![0u8; size as usize];
        if let Err(e) = handle.file.seek(SeekFrom::Start(offset)) {
            error!("seek error: {}", e);
            reply.error(Errno::EIO);
            return;
        }
        match handle.file.read(&mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => {
                error!("read error: {}", e);
                reply.error(Errno::EIO);
            }
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
        self.file_handles.lock().unwrap().remove(&fh.0);

        // Update cache DB with final file state
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            inodes.get_path(ino.0).map(|p| p.to_string())
        };
        if let Some(rel) = rel_path {
            let cache_path = self.cache_dir.join(&rel);
            if cache_path.exists() {
                if let Ok(meta) = fs::metadata(&cache_path) {
                    let mtime = meta.modified()
                        .unwrap_or(UNIX_EPOCH)
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs_f64();
                    self.db_add(&rel, meta.len(), mtime);
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

        // Mark as pending NAS write
        self.pending_writes.lock().unwrap().insert(child_rel.clone());
        if let Some(ref db) = self.db {
            let db = db.lock().unwrap();
            db.add_pending_write(&child_rel).ok();
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
            .insert(fh, OpenFileHandle { file, is_cached: true });

        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
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

        // Check that merged directory is empty
        let children = self.list_dir(&child_rel);
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
        let color = if self.is_pending_write(&rel_path) {
            Some(FINDER_COLOR_ORANGE)
        } else if self.is_cached(&rel_path) || self.is_file_cached(&rel_path) {
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

        if self.is_pending_write(&rel_path) || self.is_cached(&rel_path) || self.is_file_cached(&rel_path) {
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
        let stv = unsafe {
            let mut buf: libc::statfs = std::mem::zeroed();
            let c_path = std::ffi::CString::new(
                self.nas_path.as_os_str().as_encoded_bytes()
            ).unwrap_or_default();
            if libc::statfs(c_path.as_ptr(), &mut buf) == 0 {
                buf
            } else {
                // Fallback if NAS unavailable
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

/// Mount the PhotoCacheFS at the given mount point.
/// If the mount point is already a FUSE mount, unmount it first.
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

    let filesystem = PhotoCacheFS::new(nas_path, cache_dir, db, Some(worker), Some(flush_worker));
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("photocache".to_string()),
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

        let fs = PhotoCacheFS::new(nas.clone(), cache.clone(), None, None, None);
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

        let fs = PhotoCacheFS::new(nas.clone(), cache.clone(), None, None, None);
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

        let fs = PhotoCacheFS::new(nas, cache, None, None, None);
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

        let fs = PhotoCacheFS::new(nas, cache, None, None, None);
        let entries = fs.list_dir("");
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

        let fs = PhotoCacheFS::new(nas, cache, None, None, None);
        let entries = fs.list_dir("");
        let names: Vec<String> = entries.into_iter().map(|(n, _)| n).collect();
        assert_eq!(names.iter().filter(|n| *n == "photo.jpg").count(), 1);
    }

    #[test]
    fn test_join_rel() {
        assert_eq!(PhotoCacheFS::join_rel("", "file.jpg"), "file.jpg");
        assert_eq!(
            PhotoCacheFS::join_rel("March 2026", "IMG_001.jpg"),
            "March 2026/IMG_001.jpg"
        );
    }

    // --- is_hidden_entry tests ---

    #[test]
    fn test_is_hidden_entry_synology() {
        assert!(PhotoCacheFS::is_hidden_entry("@eaDir"));
    }

    #[test]
    fn test_is_hidden_entry_resource_fork() {
        assert!(PhotoCacheFS::is_hidden_entry("._IMG_001.jpg"));
        assert!(PhotoCacheFS::is_hidden_entry("._photo.heic"));
    }

    #[test]
    fn test_is_hidden_entry_syno_files() {
        assert!(PhotoCacheFS::is_hidden_entry("@SynoResource"));
        assert!(PhotoCacheFS::is_hidden_entry("file@SynoExt"));
    }

    #[test]
    fn test_is_hidden_entry_normal_files() {
        assert!(!PhotoCacheFS::is_hidden_entry("IMG_001.jpg"));
        assert!(!PhotoCacheFS::is_hidden_entry("March 2026"));
        assert!(!PhotoCacheFS::is_hidden_entry(".hidden_dir"));
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

        let fsys = PhotoCacheFS::new(nas, cache, None, None, None);
        assert!(fsys.is_file_cached("March 2026/IMG_001.jpg"));
    }

    #[test]
    fn test_is_file_cached_without_cached_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let fsys = PhotoCacheFS::new(nas, cache, None, None, None);
        assert!(!fsys.is_file_cached("March 2026/IMG_001.jpg"));
    }

    #[test]
    fn test_is_file_cached_rejects_root_and_bare_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let fsys = PhotoCacheFS::new(nas, cache, None, None, None);
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

        let fsys = PhotoCacheFS::new(nas, cache, Some(db), None, None);
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

        let fsys = PhotoCacheFS::new(nas, cache, Some(db), None, None);
        assert!(fsys.is_cached("March 2026/IMG_001.jpg"));
    }

    #[test]
    fn test_is_cached_uncached_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nas = tmp.path().join("nas");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&nas).unwrap();
        fs::create_dir_all(&cache).unwrap();

        let fsys = PhotoCacheFS::new(nas, cache, None, None, None);
        assert!(!fsys.is_cached("Uncached Dir"));
        assert!(!fsys.is_cached("Uncached Dir/photo.jpg"));
        assert!(!fsys.is_cached(""), "root should not be cached");
    }

    // --- parent_dir tests ---

    #[test]
    fn test_parent_dir_with_file_in_dir() {
        assert_eq!(PhotoCacheFS::parent_dir("March 2026/IMG_001.jpg"), Some("March 2026"));
    }

    #[test]
    fn test_parent_dir_bare_name() {
        assert_eq!(PhotoCacheFS::parent_dir("toplevel"), None);
    }

    #[test]
    fn test_parent_dir_empty() {
        assert_eq!(PhotoCacheFS::parent_dir(""), None);
    }

    #[test]
    fn test_parent_dir_nested() {
        // Returns the immediate parent directory
        assert_eq!(PhotoCacheFS::parent_dir("a/b/c"), Some("a/b"));
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
        let info = make_finder_info(FINDER_COLOR_YELLOW);
        assert_eq!(info[9], FINDER_COLOR_YELLOW);
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

        let fsys = PhotoCacheFS::new(nas, cache, None, None, None);
        let entries = fsys.list_dir("");
        let names: Vec<String> = entries.into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"photo.jpg".to_string()));
        assert!(!names.contains(&"@eaDir".to_string()));
        assert!(!names.contains(&"._photo.jpg".to_string()));
    }
}
