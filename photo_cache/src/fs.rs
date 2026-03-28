// src/fs.rs — FUSE filesystem presenting a unified view of NAS + local cache
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use libc::{EIO, ENOENT, ENOTEMPTY};
use log::{debug, error, warn};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;

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
}

pub struct PhotoCacheFS {
    nas_path: PathBuf,
    cache_dir: PathBuf,
    inodes: Mutex<InodeMap>,
    file_handles: Mutex<HashMap<u64, OpenFileHandle>>,
    next_fh: Mutex<u64>,
}

impl PhotoCacheFS {
    pub fn new(nas_path: PathBuf, cache_dir: PathBuf) -> Self {
        PhotoCacheFS {
            nas_path,
            cache_dir,
            inodes: Mutex::new(InodeMap::new()),
            file_handles: Mutex::new(HashMap::new()),
            next_fh: Mutex::new(1),
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
    fn make_attr(&self, ino: u64, meta: &std::fs::Metadata) -> FileAttr {
        let kind = if meta.is_dir() {
            FileType::Directory
        } else if meta.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };

        let atime = meta.accessed().unwrap_or(UNIX_EPOCH);
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        let ctime = SystemTime::UNIX_EPOCH
            + Duration::from_secs(meta.ctime() as u64);
        let crtime = meta.created().unwrap_or(UNIX_EPOCH);

        FileAttr {
            ino,
            size: meta.len(),
            blocks: meta.blocks(),
            atime,
            mtime,
            ctime,
            crtime,
            kind,
            perm: meta.mode() as u16,
            nlink: meta.nlink() as u32,
            uid: meta.uid(),
            gid: meta.gid(),
            rdev: meta.rdev() as u32,
            blksize: meta.blksize() as u32,
            flags: 0,
        }
    }

    /// List children of a directory by merging NAS + cache entries.
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
                    let ft = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    entries.insert(name.to_string(), ft);
                }
            }
        }

        entries.into_iter().collect()
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
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);

        let resolved = match self.resolve(&child_rel) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let meta = match fs::metadata(&resolved) {
            Ok(m) => m,
            Err(_) => {
                reply.error(ENOENT);
                return;
            }
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
        let attr = self.make_attr(ino, &meta);
        reply.entry(&TTL, &attr, 0);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
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
                        ino: ROOT_INO,
                        size: 0,
                        blocks: 0,
                        atime: now,
                        mtime: now,
                        ctime: now,
                        crtime: now,
                        kind: FileType::Directory,
                        perm: 0o755,
                        nlink: 2,
                        uid: unsafe { libc::getuid() },
                        gid: unsafe { libc::getgid() },
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
                reply.error(ENOENT);
                return;
            }
        };

        match fs::metadata(&resolved) {
            Ok(m) => {
                let attr = self.make_attr(ino, &m);
                reply.attr(&TTL, &attr);
            }
            Err(_) => reply.error(ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let mut entries: Vec<(u64, FileType, String)> = Vec::new();

        // "." and ".."
        entries.push((ino, FileType::Directory, ".".to_string()));
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

        // Merged directory listing
        let children = self.list_dir(&rel_path);
        for (name, ft) in children {
            let child_rel = Self::join_rel(&rel_path, &name);
            let child_ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
            entries.push((child_ino, ft, name));
        }

        for (i, (ino, ft, name)) in entries.iter().enumerate().skip(offset as usize) {
            // reply.add returns true when buffer is full
            if reply.add(*ino, (i + 1) as i64, *ft, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let resolved = match self.resolve(&rel_path) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let file = match OpenOptions::new()
            .read(true)
            .write(flags & libc::O_WRONLY != 0 || flags & libc::O_RDWR != 0)
            .open(&resolved)
        {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to open {:?}: {}", resolved, e);
                reply.error(EIO);
                return;
            }
        };

        let fh = self.alloc_fh();
        self.file_handles
            .lock()
            .unwrap()
            .insert(fh, OpenFileHandle { file });

        reply.opened(fh, 0);
    }

    fn read(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let mut handles = self.file_handles.lock().unwrap();
        let handle = match handles.get_mut(&fh) {
            Some(h) => h,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        let mut buf = vec![0u8; size as usize];
        if let Err(e) = handle.file.seek(SeekFrom::Start(offset as u64)) {
            error!("seek error: {}", e);
            reply.error(EIO);
            return;
        }
        match handle.file.read(&mut buf) {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => {
                error!("read error: {}", e);
                reply.error(EIO);
            }
        }
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.file_handles.lock().unwrap().remove(&fh);
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(EIO);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);
        let nas_path = self.nas_path.join(&child_rel);
        let cache_path = self.cache_dir.join(&child_rel);

        // Create parent directories on both sides
        if let Some(p) = nas_path.parent() {
            fs::create_dir_all(p).ok();
        }
        if let Some(p) = cache_path.parent() {
            fs::create_dir_all(p).ok();
        }

        // Write-through: create on NAS first
        let nas_file = match File::create(&nas_path) {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to create on NAS {:?}: {}", nas_path, e);
                reply.error(EIO);
                return;
            }
        };
        drop(nas_file);

        // Also create in cache
        if let Err(e) = File::create(&cache_path) {
            warn!("Failed to create cache copy {:?}: {}", cache_path, e);
        }

        // Set permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            fs::set_permissions(&nas_path, perms.clone()).ok();
            fs::set_permissions(&cache_path, perms).ok();
        }

        // Open the cache copy for subsequent read/write
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cache_path)
        {
            Ok(f) => f,
            Err(_) => match OpenOptions::new().read(true).write(true).open(&nas_path) {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to open created file: {}", e);
                    reply.error(EIO);
                    return;
                }
            },
        };

        let meta = match fs::metadata(&cache_path)
            .or_else(|_| fs::metadata(&nas_path))
        {
            Ok(m) => m,
            Err(_) => {
                reply.error(EIO);
                return;
            }
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
        let attr = self.make_attr(ino, &meta);
        let fh = self.alloc_fh();
        self.file_handles
            .lock()
            .unwrap()
            .insert(fh, OpenFileHandle { file });

        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        // Write-through: write to NAS, then update cache copy
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        // Write to NAS
        let nas_path = self.nas_path.join(&rel_path);
        if nas_path.exists() {
            if let Ok(mut f) = OpenOptions::new().write(true).open(&nas_path) {
                if f.seek(SeekFrom::Start(offset as u64)).is_ok() {
                    let _ = f.write_all(data);
                }
            }
        }

        // Write to cache via file handle
        let mut handles = self.file_handles.lock().unwrap();
        let handle = match handles.get_mut(&fh) {
            Some(h) => h,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        if let Err(e) = handle.file.seek(SeekFrom::Start(offset as u64)) {
            error!("write seek error: {}", e);
            reply.error(EIO);
            return;
        }
        match handle.file.write(data) {
            Ok(n) => reply.written(n as u32),
            Err(e) => {
                error!("write error: {}", e);
                reply.error(EIO);
            }
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(EIO);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
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
            reply.error(EIO);
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
                reply.error(EIO);
                return;
            }
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_rel);
        let attr = self.make_attr(ino, &meta);
        reply.entry(&TTL, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(EIO);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);

        // Delete from NAS
        let nas_path = self.nas_path.join(&child_rel);
        if nas_path.exists() {
            if let Err(e) = fs::remove_file(&nas_path) {
                error!("unlink NAS failed {:?}: {}", nas_path, e);
                reply.error(EIO);
                return;
            }
        }

        // Delete from cache
        let cache_path = self.cache_dir.join(&child_rel);
        if cache_path.exists() {
            fs::remove_file(&cache_path).ok();
        }

        self.inodes.lock().unwrap().remove_path(&child_rel);
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(EIO);
                return;
            }
        };

        let parent_rel = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(parent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let child_rel = Self::join_rel(&parent_rel, name_str);

        // Check that merged directory is empty
        let children = self.list_dir(&child_rel);
        if !children.is_empty() {
            reply.error(ENOTEMPTY);
            return;
        }

        // Remove from NAS
        let nas_dir = self.nas_path.join(&child_rel);
        if nas_dir.exists() {
            if let Err(e) = fs::remove_dir(&nas_dir) {
                error!("rmdir NAS failed {:?}: {}", nas_dir, e);
                reply.error(EIO);
                return;
            }
        }

        // Remove from cache
        let cache_dir = self.cache_dir.join(&child_rel);
        if cache_dir.exists() {
            fs::remove_dir(&cache_dir).ok();
        }

        self.inodes.lock().unwrap().remove_path(&child_rel);
        reply.ok();
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let name_str = match name.to_str() {
            Some(n) => n,
            None => {
                reply.error(EIO);
                return;
            }
        };
        let newname_str = match newname.to_str() {
            Some(n) => n,
            None => {
                reply.error(EIO);
                return;
            }
        };

        let (parent_rel, newparent_rel) = {
            let inodes = self.inodes.lock().unwrap();
            let p = match inodes.get_path(parent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };
            let np = match inodes.get_path(newparent) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            };
            (p, np)
        };

        let old_rel = Self::join_rel(&parent_rel, name_str);
        let new_rel = Self::join_rel(&newparent_rel, newname_str);

        // Rename on NAS
        let old_nas = self.nas_path.join(&old_rel);
        let new_nas = self.nas_path.join(&new_rel);
        if old_nas.exists() {
            if let Some(p) = new_nas.parent() {
                fs::create_dir_all(p).ok();
            }
            if let Err(e) = fs::rename(&old_nas, &new_nas) {
                error!("rename NAS failed: {}", e);
                reply.error(EIO);
                return;
            }
        }

        // Rename in cache
        let old_cache = self.cache_dir.join(&old_rel);
        let new_cache = self.cache_dir.join(&new_rel);
        if old_cache.exists() {
            if let Some(p) = new_cache.parent() {
                fs::create_dir_all(p).ok();
            }
            fs::rename(&old_cache, &new_cache).ok();
        }

        self.inodes.lock().unwrap().rename(&old_rel, &new_rel);
        reply.ok();
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let rel_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        let resolved = match self.resolve(&rel_path) {
            Some(p) => p,
            None => {
                reply.error(ENOENT);
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
            Err(_) => reply.error(EIO),
        }
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        // Report stats based on the NAS path filesystem
        // Use a reasonable default
        reply.statfs(
            0,          // blocks
            0,          // bfree
            0,          // bavail
            0,          // files
            0,          // ffree
            512,        // bsize
            255,        // namelen
            0,          // frsize
        );
    }
}

/// Mount the PhotoCacheFS at the given mount point.
pub fn mount(nas_path: PathBuf, cache_dir: PathBuf, mount_point: &Path) {
    fs::create_dir_all(mount_point).ok();
    fs::create_dir_all(&cache_dir).ok();
    let filesystem = PhotoCacheFS::new(nas_path, cache_dir);
    let options = vec![
        MountOption::FSName("photocache".to_string()),
        MountOption::AutoUnmount,
    ];
    fuser::mount2(filesystem, mount_point, &options).unwrap();
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

        let fs = PhotoCacheFS::new(nas.clone(), cache.clone());
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

        let fs = PhotoCacheFS::new(nas.clone(), cache.clone());
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

        let fs = PhotoCacheFS::new(nas, cache);
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

        let fs = PhotoCacheFS::new(nas, cache);
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

        let fs = PhotoCacheFS::new(nas, cache);
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
}
