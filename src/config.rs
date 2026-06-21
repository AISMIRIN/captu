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

#[derive(Debug, Deserialize, Clone)]
pub struct CaptureConfig {
    /// Number of contact-sheet thumbnails per caption.
    pub thumb_count: u32,
    /// Output width in pixels (display aspect; terrestrial 1440x1080 is scaled to 1920x1080).
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// JPEG quality passed to ffmpeg -q:v (2 = near-lossless, lower = better).
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
