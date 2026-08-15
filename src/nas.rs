// src/nas.rs — NAS reachability circuit breaker.
//
// fuser dispatches FUSE requests on a single thread. Any FUSE callback that
// reaches through to the NAS with a blocking `std::fs` call will wedge that
// thread if the backing NFS mount hangs (VPN drop on a hard mount → syscalls
// block forever). One wedged callback freezes the whole volume, so Finder
// stops responding on everything, cached or not.
//
// NasHealth bounds every NAS filesystem access: the blocking call runs on a
// throwaway worker thread and we wait on it with a timeout. On timeout we flip
// an atomic "NAS down" flag (the breaker) so subsequent callbacks skip the NAS
// entirely and serve cached content instead of blocking. A background probe
// thread flips the flag back once the NAS returns.

use log::{info, warn};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub struct NasHealth {
    up: Arc<AtomicBool>,
    /// Per-operation timeout for NAS filesystem calls issued from FUSE threads.
    op_timeout: Duration,
}

impl NasHealth {
    /// Probe the NAS once (bounded), set the initial state, and spawn a
    /// background thread that keeps the up/down flag current.
    ///
    /// `op_timeout` caps how long a single FUSE-thread NAS call may block.
    /// `probe_interval` is how often the background thread re-checks the NAS.
    pub fn spawn(nas_root: PathBuf, op_timeout: Duration, probe_interval: Duration) -> Self {
        let initial = probe_bounded(&nas_root, op_timeout);
        if initial {
            info!("NAS reachable: {}", nas_root.display());
        } else {
            warn!(
                "NAS not reachable at startup: {} — serving cache only until it returns",
                nas_root.display()
            );
        }
        let up = Arc::new(AtomicBool::new(initial));

        // Single long-lived probe thread. It uses a plain blocking metadata
        // call: if the mount is hung it simply parks here (one thread) until the
        // NAS returns, at which point it flips the flag back to up. It never
        // spawns per-probe threads, so a long outage cannot leak threads.
        {
            let up = up.clone();
            let nas_root = nas_root.clone();
            let _ = thread::Builder::new()
                .name("nas-health".into())
                .spawn(move || loop {
                    thread::sleep(probe_interval);
                    let ok = std::fs::metadata(&nas_root).is_ok();
                    let was = up.swap(ok, Ordering::SeqCst);
                    if was != ok {
                        if ok {
                            info!("NAS reachable again: {}", nas_root.display());
                        } else {
                            warn!("NAS unreachable: {} — serving cache only", nas_root.display());
                        }
                    }
                });
        }

        NasHealth { up, op_timeout }
    }

    /// Whether the NAS is currently believed reachable.
    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }

    /// Trip the breaker: mark the NAS down after an operation timed out.
    fn trip(&self) {
        if self.up.swap(false, Ordering::SeqCst) {
            warn!("NAS operation timed out — marking NAS down, serving cache only");
        }
    }

    /// Run a NAS filesystem operation with the breaker and a hard timeout.
    ///
    /// Returns `None` if the NAS is currently down, if the op times out, or if
    /// the op itself returns an error (e.g. the path legitimately does not
    /// exist). Crucially, this never blocks the caller longer than `op_timeout`.
    pub fn run<T, F>(&self, f: F) -> Option<T>
    where
        F: FnOnce() -> std::io::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        // Breaker open: don't even touch the NAS.
        if !self.is_up() {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        // Detached worker. If the NAS is hung, this thread parks in the syscall
        // until the kernel gives up (soft mount) or the mount recovers. Because
        // the breaker trips on the first timeout, we stop spawning more, so at
        // most one such worker is outstanding per outage.
        if thread::Builder::new()
            .name("nas-io".into())
            .spawn(move || {
                let _ = tx.send(f());
            })
            .is_err()
        {
            return None;
        }
        match rx.recv_timeout(self.op_timeout) {
            Ok(Ok(v)) => Some(v),
            Ok(Err(_)) => None, // NAS answered, but the op failed (e.g. ENOENT)
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.trip();
                None
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Bounded `fs::metadata` on the NAS. `None` when down / timed out / absent.
    pub fn metadata(&self, path: &Path) -> Option<std::fs::Metadata> {
        let p = path.to_path_buf();
        self.run(move || std::fs::metadata(&p))
    }

    /// Bounded existence check on the NAS.
    pub fn exists(&self, path: &Path) -> bool {
        self.metadata(path).is_some()
    }
}

/// One-shot bounded probe used to set the initial state without ever blocking
/// startup (important when launchd starts us before the VPN/NAS is up).
fn probe_bounded(nas_root: &Path, timeout: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    let root = nas_root.to_path_buf();
    if thread::Builder::new()
        .name("nas-probe".into())
        .spawn(move || {
            let _ = tx.send(std::fs::metadata(&root).is_ok());
        })
        .is_err()
    {
        return false;
    }
    matches!(rx.recv_timeout(timeout), Ok(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_when_root_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let health = NasHealth::spawn(
            tmp.path().to_path_buf(),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        assert!(health.is_up());
        // A real file resolves through the breaker.
        std::fs::write(tmp.path().join("a.txt"), b"hi").unwrap();
        assert!(health.metadata(&tmp.path().join("a.txt")).is_some());
        // A missing file returns None without tripping the breaker.
        assert!(health.metadata(&tmp.path().join("missing.txt")).is_none());
        assert!(health.is_up());
    }

    #[test]
    fn down_when_root_missing() {
        let health = NasHealth::spawn(
            PathBuf::from("/nonexistent/nfs/root"),
            Duration::from_secs(2),
            Duration::from_secs(60),
        );
        assert!(!health.is_up());
        // Breaker open → run() short-circuits to None without touching the FS.
        assert!(health.metadata(&PathBuf::from("/nonexistent/nfs/root/x")).is_none());
    }
}
