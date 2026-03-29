mod cache_db;
mod config;
mod fs;
mod sync;

use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "nfscache",
    about = "Browse your NAS photo library as if it were local",
    version,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Mount the FUSE filesystem
    Mount,
    /// Unmount the filesystem
    Unmount,
    /// Show cache status
    Status,
    /// Initialize config and cache directories
    Init,
    /// Wipe local cache (unmount first)
    Clear,
}

fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".nfscache/config.json")
}

fn is_mounted(config: &Config) -> bool {
    std::process::Command::new("mount")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.contains(&config.mount_point.to_string_lossy().to_string()))
        .unwrap_or(false)
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();
    let cp = config_path();
    let config = Config::load(&cp);

    match cli.command {
        Commands::Mount => cmd_mount(&config, &cp),
        Commands::Unmount => cmd_unmount(&config),
        Commands::Status => cmd_status(&config),
        Commands::Init => cmd_init(&config),
        Commands::Clear => cmd_clear(&config),
    }
}

// --- Mount ---

fn cmd_mount(config: &Config, cp: &std::path::Path) {
    // First-run: suggest init if no config exists
    if !cp.exists() {
        eprintln!("No config found. Creating default config...");
        cmd_init(config);
        eprintln!();
    }

    // Validate NAS path
    if !config.nas_photos_path.exists() {
        eprintln!("Error: NAS path not found: {}", config.nas_photos_path.display());
        eprintln!("Mount your NAS first:");
        eprintln!("  sudo mount -t nfs -o vers=3,nolock,resvport <NAS_IP>:<SHARE> <MOUNT_POINT>");
        eprintln!("Then update nas_photos_path in {}", cp.display());
        std::process::exit(1);
    }

    // Validate cache size
    if config.max_cache_bytes < 100_000_000 {
        eprintln!("Error: max_cache_bytes must be at least 100 MB (got {} bytes)", config.max_cache_bytes);
        std::process::exit(1);
    }

    std::fs::create_dir_all(&config.cache_dir).ok();

    println!("nfscache v{}", env!("CARGO_PKG_VERSION"));
    println!("  NAS:    {}", config.nas_photos_path.display());
    println!("  Cache:  {} ({:.1} GB limit)", config.cache_dir.display(), config.max_cache_bytes as f64 / 1e9);
    println!("  Mount:  {}", config.mount_point.display());
    println!();
    println!("Directories cache on demand as you browse. Ctrl+C to unmount.");

    fs::mount(
        config.nas_photos_path.clone(),
        config.cache_dir.clone(),
        &config.mount_point,
        &config.db_path,
        config.max_cache_bytes,
    );
}

// --- Unmount ---

fn cmd_unmount(config: &Config) {
    if !is_mounted(config) {
        println!("Not mounted.");
        return;
    }
    match std::process::Command::new("umount")
        .arg(&config.mount_point)
        .status()
    {
        Ok(s) if s.success() => println!("Unmounted {}", config.mount_point.display()),
        Ok(_) => {
            eprintln!("Failed to unmount. Try: umount -f {}", config.mount_point.display());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to run umount: {}", e);
            std::process::exit(1);
        }
    }
}

// --- Status ---

fn dir_disk_usage(path: &std::path::Path) -> (u64, usize) {
    let mut size = 0u64;
    let mut count = 0usize;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.filter_map(|e| e.ok()) {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    size += meta.len();
                    count += 1;
                } else if meta.is_dir() {
                    let (s, c) = dir_disk_usage(&entry.path());
                    size += s;
                    count += c;
                }
            }
        }
    }
    (size, count)
}

fn cmd_status(config: &Config) {
    println!("nfscache v{}", env!("CARGO_PKG_VERSION"));
    println!();

    // Mount status
    if is_mounted(config) {
        println!("  Status: mounted at {}", config.mount_point.display());
    } else {
        println!("  Status: not mounted");
    }

    // NAS status
    if config.nas_photos_path.exists() {
        println!("  NAS:    {} (accessible)", config.nas_photos_path.display());
    } else {
        println!("  NAS:    {} (not accessible)", config.nas_photos_path.display());
    }

    // Cache usage from disk
    let (disk_total, disk_files) = if config.cache_dir.is_dir() {
        dir_disk_usage(&config.cache_dir)
    } else {
        (0, 0)
    };
    let pct = if config.max_cache_bytes > 0 {
        (disk_total as f64 / config.max_cache_bytes as f64 * 100.0).min(100.0)
    } else {
        0.0
    };
    println!(
        "  Cache:  {:.2} GB / {:.1} GB ({:.0}%, {} files)",
        disk_total as f64 / 1e9,
        config.max_cache_bytes as f64 / 1e9,
        pct,
        disk_files,
    );
    println!();

    let db = match cache_db::CacheDB::open(&config.db_path) {
        Ok(db) => db,
        Err(_) => {
            println!("No cache database found. Run 'nfscache init' first.");
            return;
        }
    };

    // Cached directories
    let cached_dirs = db.lru_directories().unwrap_or_default();
    let mut shown_dirs = Vec::new();
    for dir in cached_dirs.iter().rev() {
        let dir_path = config.cache_dir.join(&dir.dir_path);
        let (actual_size, file_count) = dir_disk_usage(&dir_path);
        if file_count > 0 {
            shown_dirs.push((&dir.dir_path, actual_size, file_count));
        } else {
            db.remove_dir(&dir.dir_path).ok();
        }
    }
    if !shown_dirs.is_empty() {
        println!("Cached directories:");
        for (path, size, count) in &shown_dirs {
            println!(
                "  {} ({:.1} MB, {} files)",
                path,
                *size as f64 / 1e6,
                count,
            );
        }
        println!();
    }

    // Partial directories
    if config.cache_dir.is_dir() {
        let cached_names: std::collections::HashSet<&str> =
            cached_dirs.iter().map(|d| d.dir_path.as_str()).collect();
        let mut partial = Vec::new();
        fn collect_partial(
            base: &std::path::Path,
            rel: &str,
            cached: &std::collections::HashSet<&str>,
            out: &mut Vec<(String, usize)>,
        ) {
            let dir = if rel.is_empty() { base.to_path_buf() } else { base.join(rel) };
            let entries: Vec<_> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .collect();
            let file_count = entries.iter()
                .filter(|e| {
                    e.file_type().map(|t| t.is_file()).unwrap_or(false)
                    && e.file_name() != ".DS_Store"
                })
                .count();
            let rel_str = rel.to_string();
            if !rel.is_empty() && !cached.contains(rel_str.as_str()) && file_count > 0 {
                out.push((rel_str.clone(), file_count));
            }
            for entry in &entries {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let child_rel = if rel.is_empty() {
                        name
                    } else {
                        format!("{}/{}", rel, name)
                    };
                    collect_partial(base, &child_rel, cached, out);
                }
            }
        }
        collect_partial(&config.cache_dir, "", &cached_names, &mut partial);
        if !partial.is_empty() {
            println!("Partially cached:");
            for (path, count) in &partial {
                println!("  {} ({} files)", path, count);
            }
            println!();
        }
    }

    // Pending writes
    let pending = db.all_pending_writes().unwrap_or_default();
    if !pending.is_empty() {
        println!("Pending NAS writes: {}", pending.len());
        for path in &pending {
            println!("  {}", path);
        }
        println!();
    }
}

// --- Clear ---

fn cmd_clear(config: &Config) {
    if is_mounted(config) {
        eprintln!("Error: filesystem is mounted. Run 'nfscache unmount' first.");
        std::process::exit(1);
    }

    let (size_before, files_before) = if config.cache_dir.is_dir() {
        dir_disk_usage(&config.cache_dir)
    } else {
        (0, 0)
    };

    if config.cache_dir.exists() {
        std::fs::remove_dir_all(&config.cache_dir).ok();
        std::fs::create_dir_all(&config.cache_dir).ok();
    }
    if config.db_path.exists() {
        std::fs::remove_file(&config.db_path).ok();
    }

    println!(
        "Cache cleared. Freed {:.2} GB ({} files).",
        size_before as f64 / 1e9,
        files_before,
    );
}

// --- Init ---

fn cmd_init(config: &Config) {
    let cp = config_path();
    if let Some(parent) = cp.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::create_dir_all(&config.cache_dir).ok();

    if !cp.exists() {
        match serde_json::to_string_pretty(&config) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&cp, json) {
                    eprintln!("Failed to write config: {}", e);
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("Failed to serialize config: {}", e);
                std::process::exit(1);
            }
        }
        println!("Config created at {}", cp.display());
    } else {
        println!("Config exists at {}", cp.display());
    }

    println!("  NAS path:   {}", config.nas_photos_path.display());
    println!("  Cache dir:  {}", config.cache_dir.display());
    println!("  Mount point: {}", config.mount_point.display());
    println!("  Cache limit: {:.1} GB", config.max_cache_bytes as f64 / 1e9);

    if !config.nas_photos_path.exists() {
        println!("\n  Note: NAS path does not exist yet. Mount your NAS before running 'nfscache mount'.");
    }
}
