# photocache

A FUSE filesystem that presents a unified view of your NAS photos with on-demand local caching. Open a photo and its entire directory gets cached locally for instant access. Least-recently-used directories are evicted when the cache is full.

## How it works

- Mounts a virtual directory (e.g. `~/NAS Pictures`) showing **all** NAS photos
- **On-demand caching**: opening any photo triggers caching of its entire directory
- **LRU eviction**: least-recently-accessed directories are evicted when cache exceeds the limit
- **Write-local-first**: new photos are written to local cache instantly, then flushed to NAS in the background (every 5 seconds)
- **Finder tags**: green dot = cached, orange dot = pending NAS write, no dot = NAS-only
- **NAS change detection**: directories moved/deleted on the NAS are detected and cache invalidated within 60 seconds
- Synology metadata (`@eaDir`), macOS resource forks (`._*`), and `.DS_Store` files are hidden

## Prerequisites

macFUSE is required:

```bash
brew install macfuse
# Allow the kernel extension in System Settings > Privacy & Security
# Reboot
```

## Install

```bash
make install
```

This builds the release binary, copies it to `/usr/local/bin/`, and initializes the config.

## Setup

```bash
# 1. Mount your NAS (if not already)
sudo mount -v -t nfs -o vers=3,nolock,resvport 192.168.50.21:/volume1/media /Users/johng/nas_media

# 2. Mount the virtual filesystem
photocache mount
```

Directories are cached automatically as you browse and open photos.

## Make targets

| Target | Description |
|--------|-------------|
| `make build` | Build release binary |
| `make install` | Build, install to `/usr/local/bin`, init config |
| `make uninstall` | Stop service, remove binary and plist |
| `make service-start` | Install and start launchd service |
| `make service-stop` | Stop and remove launchd service |
| `make service-restart` | Restart launchd service |
| `make test` | Run tests |
| `make clean` | Remove build artifacts |

## Commands

| Command | Description |
|---------|-------------|
| `photocache init` | Create config file and cache directories |
| `photocache status` | Show cache usage, cached/partial directories, pending writes |
| `photocache mount` | Mount the FUSE filesystem |
| `photocache unmount` | Unmount the filesystem |
| `photocache clear` | Wipe the local cache (must unmount first) |

## Configuration

Config lives at `~/.photo_cache/config.json`:

```json
{
  "nas_photos_path": "/Users/johng/nas_media/photos",
  "cache_dir": "/Users/johng/.photo_cache/data",
  "db_path": "/Users/johng/.photo_cache/cache.db",
  "mount_point": "/Users/johng/NAS Pictures",
  "max_cache_bytes": 53687091200
}
```

- `max_cache_bytes` — Local cache size limit (default: 50GB)

## Run as a background service

```bash
make service-start
```

To stop:

```bash
make service-stop
```

## Logging

```bash
# Cache operations only (recommended)
RUST_LOG=photocache::sync=info photocache mount

# Per-file detail
RUST_LOG=photocache::sync=debug photocache mount

# All modules
RUST_LOG=photocache=debug photocache mount
```

When running as a launchd service, logs go to:
- `~/Library/Logs/photocache/mount.log`
- `~/Library/Logs/photocache/mount.err.log`

## Architecture

```
~/NAS Pictures (FUSE mount)
    |
    +-- Read --> Check local cache --> HIT: serve from disk
    |                              +-> MISS: read from NAS
    |
    +-- Write --> Write to local cache --> Background flush to NAS (every 5s)
    |
    +-- Directory listing --> Merge NAS + cache entries (sorted, deduped)
    |
    +-- Finder tags --> Green: cached, Orange: pending NAS write

Background workers:
    Cache worker: caches directories on demand, evicts LRU when over budget
    Flush worker: syncs pending writes to NAS, runs eviction, validates NAS state

Startup cleanup:
    - Removes partial caches from interrupted sessions
    - Removes cached files deleted from NAS (preserves pending writes)
    - Cleans stale DB entries
```

## Supported formats

jpg, jpeg, png, heic, heif, dng, raw, tiff, tif, cr2, nef, arw, aae, xmp, mov
