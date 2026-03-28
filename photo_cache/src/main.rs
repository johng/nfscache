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
    /// Show cache status
    Status,
    /// Run sync immediately
    Sync,
    /// Wipe local cache
    Clear,
    /// Mount FUSE filesystem
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
        Commands::Sync => cmd_sync(&config),
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
    println!("Cache dir: {}", config.cache_dir.display());
    println!("Mount point: {}", config.mount_point.display());
}

fn cmd_sync(config: &Config) {
    let db = cache_db::CacheDB::open(&config.db_path).expect("Failed to open cache DB");
    let engine = sync::SyncEngine::new(
        config.nas_photos_path.clone(),
        config.cache_dir.clone(),
        db,
        config.max_cache_bytes,
    );
    engine.sync();
    println!("Sync complete.");
}

fn cmd_clear(config: &Config) {
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
    println!("Press Ctrl+C to unmount.");
    fs::mount(
        config.nas_photos_path.clone(),
        config.cache_dir.clone(),
        &config.mount_point,
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
