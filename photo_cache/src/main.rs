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

fn cmd_status(config: &Config) {
    let db = cache_db::CacheDB::open(&config.db_path).expect("Failed to open cache DB");
    let total = db.total_size().unwrap_or(0);
    let paths = db.all_cached_paths().unwrap_or_default();
    println!(
        "Cache: {:.2} GB / {:.1} GB",
        total as f64 / 1e9,
        config.max_cache_bytes as f64 / 1e9
    );
    println!("Files cached: {}", paths.len());

    // Show cached directories
    let cached_dirs = db.lru_directories().unwrap_or_default();
    if !cached_dirs.is_empty() {
        println!("\nCached directories (most recent first):");
        for dir in cached_dirs.iter().rev() {
            println!(
                "  {} ({:.1} MB)",
                dir.dir_path,
                dir.total_size as f64 / 1e6
            );
        }
    }

    // Show partially cached dirs (on disk but not fully cached in DB)
    if config.cache_dir.is_dir() {
        let cached_names: std::collections::HashSet<&str> =
            cached_dirs.iter().map(|d| d.dir_path.as_str()).collect();
        let mut partial = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&config.cache_dir) {
            for entry in read_dir.filter_map(|e| e.ok()) {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !cached_names.contains(name.as_str()) {
                        let file_count = std::fs::read_dir(entry.path())
                            .map(|d| d.count())
                            .unwrap_or(0);
                        if file_count > 0 {
                            partial.push((name, file_count));
                        }
                    }
                }
            }
        }
        if !partial.is_empty() {
            println!("\nPartially cached directories:");
            for (name, count) in &partial {
                println!("  {} ({} files)", name, count);
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
