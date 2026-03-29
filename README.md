# nfscache

Browse your entire NAS photo library as if it were local. No waiting, no manual syncing, no duplicates.

nfscache is a FUSE filesystem for macOS that mounts your NAS photos as a regular folder. Photos you actually look at get cached locally for instant access. Everything else streams transparently from the NAS. Write a photo and it saves to your local disk immediately, then syncs to the NAS in the background within seconds.

**Why?** NAS photo libraries are slow to browse. Cloud sync tools copy everything. nfscache gives you the speed of local storage with the capacity of your NAS, using only the disk space you choose.

## Features

- **On-demand caching** -- open a photo and its entire directory gets cached locally
- **LRU eviction** -- least-recently-used directories are automatically evicted when the cache is full
- **Write-local-first** -- new and edited photos save instantly to cache, then flush to NAS in the background
- **Finder integration** -- green dot for cached files, orange dot for pending NAS sync
- **NAS change detection** -- files moved or deleted on the NAS are detected and cache updated within 60 seconds
- **Crash-safe** -- partial caches from interrupted sessions are cleaned up on next mount
- **Clean browsing** -- Synology metadata, macOS resource forks, and `.DS_Store` files are hidden

## Quick start

```bash
# Prerequisites: macFUSE
brew install macfuse
# Allow kernel extension in System Settings > Privacy & Security, then reboot

# Install
make install

# Mount your NAS and start browsing
sudo mount -t nfs -o vers=3,nolock,resvport 192.168.50.21:/volume1/media ~/nas_media
nfscache mount
```

Open `~/NAS Pictures` in Finder. Directories cache automatically as you browse.

## Make targets

| Target | Description |
|--------|-------------|
| `make install` | Build, install to `/usr/local/bin`, init config |
| `make upgrade` | Rebuild and replace binary, restart service |
| `make uninstall` | Stop service, remove binary |
| `make service-start` | Start as a background launchd service |
| `make service-stop` | Stop the background service |
| `make service-restart` | Restart the service |
| `make test` | Run tests |
| `make clean` | Remove build artifacts |

## Commands

| Command | Description |
|---------|-------------|
| `nfscache mount` | Mount the FUSE filesystem |
| `nfscache unmount` | Unmount the filesystem |
| `nfscache status` | Show cache usage, cached directories, pending writes |
| `nfscache init` | Create config file and cache directories |
| `nfscache clear` | Wipe the local cache (unmount first) |

## Configuration

Config is created at `~/.nfscache/config.json` on first `init`:

```json
{
  "nas_photos_path": "/Users/johng/nas_media/photos",
  "cache_dir": "/Users/johng/.nfscache/data",
  "db_path": "/Users/johng/.nfscache/cache.db",
  "mount_point": "/Users/johng/NAS Pictures",
  "max_cache_bytes": 53687091200
}
```

`max_cache_bytes` controls the local cache size limit (default 50 GB).

## Background service

Run nfscache automatically at login:

```bash
make service-start    # install and start
make service-stop     # stop
make service-restart  # restart after config changes
```

## Logging

```bash
# Recommended: cache operations
RUST_LOG=nfscache::sync=info nfscache mount

# Verbose: per-file detail
RUST_LOG=nfscache::sync=debug nfscache mount
```

Service logs: `~/Library/Logs/nfscache/`

## How it works

```
~/NAS Pictures (FUSE mount)

  Reads:    cache hit  --> local disk (fast)
            cache miss --> NAS (transparent)

  Writes:   local cache --> background flush to NAS (5s)

  Browsing: merged NAS + cache directory listings

  Tags:     green = cached    orange = pending sync

Background:
  Cache worker   -- caches directories on demand, evicts LRU
  Flush worker   -- syncs writes to NAS, validates NAS state
  Startup        -- cleans partial caches, removes stale entries
```

## Supported formats

jpg, jpeg, png, heic, heif, dng, raw, tiff, tif, cr2, nef, arw, aae, xmp, mov

## Why not rclone mount?

[rclone mount](https://rclone.org/commands/rclone_mount/) is an excellent tool for mounting cloud storage (S3, Google Drive, Dropbox) as a local folder. Its VFS cache makes remote files work with local apps. But it solves a different problem:

**rclone** connects to cloud APIs (HTTP) where random reads aren't possible, so it downloads files to a local cache to make `seek`/`read` work at all. The cache exists for *correctness*, not speed. Each file is cached individually as accessed.

**nfscache** sits on top of an existing NFS mount where reads already work. The cache exists purely for *speed*. When you open one photo, the entire directory is pre-fetched in the background so the next photo is instant. This matches how people actually browse photos — flipping through a folder, not jumping between random files.

| | nfscache | rclone mount |
|---|---|---|
| Backend | NFS mount (already works) | Cloud APIs (S3, SFTP, WebDAV) |
| Cache purpose | Speed (prefetch for browsing) | Correctness (make seeks work) |
| Cache unit | Entire directory on first access | Individual files as accessed |
| Browse 100 photos | 1 network fetch (directory) | 100 network fetches (each file) |
| Finder tags | Green/orange cache indicators | No |
| Write model | Local-first, async flush | Write-through or full cache |

Use rclone if your photos are in cloud storage. Use nfscache if they're on a NAS.

## Requirements

- macOS with [macFUSE](https://macfuse.github.io/)
- NAS accessible via NFS mount
- Rust toolchain (for building)
