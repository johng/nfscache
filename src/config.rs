use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub nas_photos_path: PathBuf,
    pub cache_dir: PathBuf,
    pub db_path: PathBuf,
    pub mount_point: PathBuf,
    pub max_cache_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            nas_photos_path: home.join("nas_media/photos"),
            cache_dir: home.join(".nfscache/data"),
            db_path: home.join(".nfscache/cache.db"),
            mount_point: home.join("Photos"),
            max_cache_bytes: 53_687_091_200, // 50 GB
        }
    }
}

impl Config {
    pub fn load(config_path: &std::path::Path) -> Self {
        let mut config = Config::default();
        if config_path.exists() {
            if let Ok(contents) = std::fs::read_to_string(config_path) {
                if let Ok(overrides) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(obj) = overrides.as_object() {
                        if let Some(v) = obj.get("nas_photos_path").and_then(|v| v.as_str()) {
                            config.nas_photos_path = PathBuf::from(v);
                        }
                        if let Some(v) = obj.get("cache_dir").and_then(|v| v.as_str()) {
                            config.cache_dir = PathBuf::from(v);
                        }
                        if let Some(v) = obj.get("db_path").and_then(|v| v.as_str()) {
                            config.db_path = PathBuf::from(v);
                        }
                        if let Some(v) = obj.get("mount_point").and_then(|v| v.as_str()) {
                            config.mount_point = PathBuf::from(v);
                        }
                        if let Some(v) = obj.get("max_cache_bytes").and_then(|v| v.as_u64()) {
                            config.max_cache_bytes = v;
                        }
                    }
                }
            }
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.max_cache_bytes, 53_687_091_200);
    }

    #[test]
    fn test_load_nonexistent() {
        let config = Config::load(std::path::Path::new("/nonexistent/config.json"));
        assert_eq!(config.max_cache_bytes, 53_687_091_200);
    }

    #[test]
    fn test_load_partial_override() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, r#"{{"max_cache_bytes": 1000}}"#).unwrap();
        let config = Config::load(f.path());
        assert_eq!(config.max_cache_bytes, 1000);
    }
}
