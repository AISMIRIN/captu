use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub paths: PathsConfig,
    pub capture: CaptureConfig,
    pub ingest: IngestConfig,
    pub server: ServerConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PathsConfig {
    pub nas_mount: String,
    pub ts_glob: String,
    pub cache_dir: String,
    pub db_path: String,
}

fn default_thumb_width() -> u32 {
    640
}
fn default_thumb_height() -> u32 {
    360
}
fn default_thumb_quality() -> u32 {
    4
}

#[derive(Debug, Deserialize, Clone)]
pub struct CaptureConfig {
    /// Number of contact-sheet thumbnails per caption.
    pub thumb_count: u32,
    /// Thumbnail width for the contact-sheet grid / preview (smaller, display-only).
    #[serde(default = "default_thumb_width")]
    pub thumb_width: u32,
    /// Thumbnail height for the contact-sheet grid / preview.
    #[serde(default = "default_thumb_height")]
    pub thumb_height: u32,
    /// JPEG quality for thumbnails passed to ffmpeg -q:v (lower = better).
    #[serde(default = "default_thumb_quality")]
    pub thumb_quality: u32,
    /// Full-resolution output width (used for download/share JPEG).
    pub width: u32,
    /// Full-resolution output height.
    pub height: u32,
    /// JPEG quality for full-resolution download passed to ffmpeg -q:v.
    pub jpeg_quality: u32,
}

fn default_concurrency() -> u32 {
    3
}

#[derive(Debug, Deserialize, Clone)]
pub struct IngestConfig {
    pub schedule_cron: String,
    pub run_on_startup: bool,
    /// Number of parallel ingest workers. Tune based on NAS bandwidth (recommended 2-4).
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
    /// Skip TS files that have no ARIB caption PID.
    #[serde(default)]
    pub require_captions: bool,
    /// Glob patterns (relative to nas_mount) to include. Empty = accept all.
    #[serde(default)]
    pub filter_include: Vec<String>,
    /// Glob patterns to exclude. Evaluated after filter_include.
    #[serde(default)]
    pub filter_exclude: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&content)?;
        // Environment variables override config.toml values, enabling dev/prod
        // path separation without editing the file (and without rebuilding).
        config.apply_env();
        Ok(config)
    }

    /// Override path settings from environment variables.
    /// CAPTU_NAS_MOUNT / CAPTU_TS_GLOB / CAPTU_DB_PATH / CAPTU_CACHE_DIR
    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("CAPTU_NAS_MOUNT") {
            self.paths.nas_mount = v;
        }
        if let Ok(v) = std::env::var("CAPTU_TS_GLOB") {
            self.paths.ts_glob = v;
        }
        if let Ok(v) = std::env::var("CAPTU_DB_PATH") {
            self.paths.db_path = v;
        }
        if let Ok(v) = std::env::var("CAPTU_CACHE_DIR") {
            self.paths.cache_dir = v;
        }
    }

    pub fn default_dev() -> Self {
        Config {
            paths: PathsConfig {
                nas_mount: ".".to_string(),
                ts_glob: "**/*.ts".to_string(),
                cache_dir: "/tmp/captu_cache".to_string(),
                db_path: "./data/captions.db".to_string(),
            },
            capture: CaptureConfig {
                thumb_count: 6,
                thumb_width: 640,
                thumb_height: 360,
                thumb_quality: 4,
                width: 1920,
                height: 1080,
                jpeg_quality: 2,
            },
            ingest: IngestConfig {
                schedule_cron: "0 * * * * *".to_string(),
                run_on_startup: true,
                concurrency: 3,
                require_captions: false,
                filter_include: vec![],
                filter_exclude: vec![],
            },
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 8000,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn default_dev_values() {
        let c = Config::default_dev();
        assert_eq!(c.capture.thumb_count, 6);
        assert_eq!(c.capture.thumb_width, 640);
        assert_eq!(c.capture.thumb_height, 360);
        assert_eq!(c.capture.thumb_quality, 4);
        assert_eq!(c.server.port, 8000);
        assert!(!c.ingest.require_captions);
        assert!(c.ingest.filter_include.is_empty());
        assert!(c.ingest.filter_exclude.is_empty());
    }

    #[test]
    fn toml_optional_fields_get_defaults() {
        let toml = r#"
[paths]
nas_mount = "/mnt/nas"
ts_glob = "**/*.ts"
cache_dir = "/tmp/cache"
db_path = "/tmp/db.sqlite"

[capture]
thumb_count = 4
width = 1920
height = 1080
jpeg_quality = 2

[ingest]
schedule_cron = "0 * * * * *"
run_on_startup = false

[server]
host = "127.0.0.1"
port = 8080
"#;
        let c: Config = toml::from_str(toml).expect("should parse");
        // serde(default) fields
        assert_eq!(c.capture.thumb_width, 640);
        assert_eq!(c.capture.thumb_height, 360);
        assert_eq!(c.capture.thumb_quality, 4);
        assert_eq!(c.ingest.concurrency, 3);
        assert!(!c.ingest.require_captions);
        assert!(c.ingest.filter_include.is_empty());
        assert!(c.ingest.filter_exclude.is_empty());
    }

    #[test]
    fn toml_missing_required_field_errors() {
        // thumb_count has no serde default → missing it must be an error
        let toml = r#"
[paths]
nas_mount = "/mnt/nas"
ts_glob = "**/*.ts"
cache_dir = "/tmp/cache"
db_path = "/tmp/db.sqlite"

[capture]
width = 1920
height = 1080
jpeg_quality = 2

[ingest]
schedule_cron = "0 * * * * *"
run_on_startup = false

[server]
host = "127.0.0.1"
port = 8080
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn toml_port_out_of_range_errors() {
        // port is u16; value > 65535 must fail to parse
        let toml = r#"
[paths]
nas_mount = "."
ts_glob = "**/*.ts"
cache_dir = "/tmp"
db_path = "/tmp/db"

[capture]
thumb_count = 4
width = 1920
height = 1080
jpeg_quality = 2

[ingest]
schedule_cron = "0 * * * * *"
run_on_startup = false

[server]
host = "127.0.0.1"
port = 99999
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }
}
