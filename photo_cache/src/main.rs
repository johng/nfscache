mod cache_db;
mod config;
mod fs;
mod sync;

use clap::{Parser, Subcommand};
use config::Config;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "photocache", about = "Photo cache manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show cache status (cached dirs, total size)
    Status,
    /// Wipe local cache
    Clear,
    /// Mount FUSE filesystem (caches directories on demand)
    Mount,
    /// Unmount FUSE filesystem
    Unmount,
    /// Initialize config and directories
    Init,
}

fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".photo_cache/config.json")
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();
    let config = Config::load(&config_path());

    match cli.command {
        Commands::Status => cmd_status(&config),
        Commands::Clear => cmd_clear(&config),
        Commands::Mount => cmd_mount(&config),
        Commands::Unmount => cmd_unmount(&config),
        Commands::Init => cmd_init(&config),
    }
}

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
    // Calculate actual disk usage
    let (disk_total, disk_files) = if config.cache_dir.is_dir() {
        dir_disk_usage(&config.cache_dir)
    } else {
        (0, 0)
    };

    println!(
        "Cache: {:.2} GB / {:.1} GB ({} files)",
        disk_total as f64 / 1e9,
        config.max_cache_bytes as f64 / 1e9,
        disk_files,
    );

    let db = cache_db::CacheDB::open(&config.db_path).expect("Failed to open cache DB");

    // Show cached directories with actual disk sizes (skip empty ones)
    let cached_dirs = db.lru_directories().unwrap_or_default();
    let mut shown_dirs = Vec::new();
    for dir in cached_dirs.iter().rev() {
        let dir_path = config.cache_dir.join(&dir.dir_path);
        let (actual_size, file_count) = dir_disk_usage(&dir_path);
        if file_count > 0 {
            shown_dirs.push((&dir.dir_path, actual_size, file_count));
        } else {
            // Clean up stale empty directory entry
            db.remove_dir(&dir.dir_path).ok();
        }
    }
    if !shown_dirs.is_empty() {
        println!("\nCached directories (most recent first):");
        for (path, size, count) in &shown_dirs {
            println!(
                "  {} ({:.1} MB, {} files)",
                path,
                *size as f64 / 1e6,
                count,
            );
        }
    }

    // Show partially cached dirs (on disk but not fully cached in DB)
    if config.cache_dir.is_dir() {
        let cached_names: std::collections::HashSet<&str> =
            cached_dirs.iter().map(|d| d.dir_path.as_str()).collect();
        let mut partial = Vec::new();
        // Walk all subdirs recursively to find leaf directories with files
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
            println!("\nPartially cached directories:");
            for (path, count) in &partial {
                println!("  {} ({} files)", path, count);
            }
        }
    }

    // Show pending writes
    let pending = db.all_pending_writes().unwrap_or_default();
    if !pending.is_empty() {
        println!("\nPending NAS writes: {}", pending.len());
        for path in &pending {
            println!("  {}", path);
        }
    }

    println!("\nCache dir: {}", config.cache_dir.display());
    println!("Mount point: {}", config.mount_point.display());
}

fn cmd_clear(config: &Config) {
    // Check if the FUSE mount is active
    let mount_check = std::process::Command::new("mount")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    if mount_check.contains(&config.mount_point.to_string_lossy().to_string()) {
        eprintln!("Warning: {} is currently mounted. Unmount first with: photocache unmount",
            config.mount_point.display());
        return;
    }

    if config.cache_dir.exists() {
        std::fs::remove_dir_all(&config.cache_dir).ok();
        std::fs::create_dir_all(&config.cache_dir).ok();
    }
    if config.db_path.exists() {
        std::fs::remove_file(&config.db_path).ok();
    }
    println!("Cache cleared.");
}

fn cmd_mount(config: &Config) {
    std::fs::create_dir_all(&config.cache_dir).ok();
    println!("Mounting at {}...", config.mount_point.display());
    println!("Directories will be cached on demand as you open photos.");
    println!("Enable cache logging with: RUST_LOG=photocache::sync=debug photocache mount");
    fs::mount(
        config.nas_photos_path.clone(),
        config.cache_dir.clone(),
        &config.mount_point,
        &config.db_path,
        config.max_cache_bytes,
    );
}

fn cmd_unmount(config: &Config) {
    std::process::Command::new("umount")
        .arg(&config.mount_point)
        .status()
        .expect("Failed to unmount");
    println!("Unmounted {}", config.mount_point.display());
}

fn cmd_init(config: &Config) {
    let cp = config_path();
    if let Some(parent) = cp.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::create_dir_all(&config.cache_dir).ok();
    if !cp.exists() {
        let json = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&cp, json).unwrap();
        println!("Created config at {}", cp.display());
    } else {
        println!("Config already exists at {}", cp.display());
    }
    println!("Cache dir: {}", config.cache_dir.display());
}
