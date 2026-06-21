use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "Usage: ingest_cli [options]\n\
             \n\
             Options:\n\
             --scan <dir>              Directory to scan (overrides config nas_mount)\n\
             --config <path>           Config file (default: config.toml)\n\
             --db <path>               DB path override\n\
             --cache-dir <path>        Cache dir override\n\
             --concurrency <n>         Worker count override\n\
             --reingest <ts-path>      Reset and re-ingest a single file\n\
             --reingest-program <id>   Reset and re-ingest all files for a program id"
        );
        return Ok(());
    }

    let config_path = args
        .windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    let mut config = if config_path.exists() {
        captu::config::Config::load(&config_path)?
    } else {
        eprintln!(
            "[config] {} not found, using defaults",
            config_path.display()
        );
        captu::config::Config::default_dev()
    };

    // CLI overrides (applied after env overrides that happen in Config::load)
    if let Some(scan_dir) = args.windows(2).find(|w| w[0] == "--scan").map(|w| &w[1]) {
        config.paths.nas_mount = scan_dir.clone();
    }
    if let Some(db) = args.windows(2).find(|w| w[0] == "--db").map(|w| &w[1]) {
        config.paths.db_path = db.clone();
    }
    if let Some(cd) = args
        .windows(2)
        .find(|w| w[0] == "--cache-dir")
        .map(|w| &w[1])
    {
        config.paths.cache_dir = cd.clone();
    }
    if let Some(c) = args
        .windows(2)
        .find(|w| w[0] == "--concurrency")
        .map(|w| &w[1])
    {
        if let Ok(n) = c.parse::<u32>() {
            config.ingest.concurrency = n;
        }
    }

    println!("[ingest_cli] scan:        {}", config.paths.nas_mount);
    println!("[ingest_cli] db:          {}", config.paths.db_path);
    println!("[ingest_cli] cache:       {}", config.paths.cache_dir);
    println!("[ingest_cli] concurrency: {}", config.ingest.concurrency);

    let pool = captu::db::init_db(&config.paths.db_path, config.ingest.concurrency + 5).await?;
    let config = Arc::new(config);
    let cache_dir = PathBuf::from(&config.paths.cache_dir);

    // Sub-command: --reingest <ts-path>
    if let Some(ts_path) = args
        .windows(2)
        .find(|w| w[0] == "--reingest")
        .map(|w| &w[1])
    {
        let ts_path = Path::new(ts_path);
        let path_str = ts_path.to_string_lossy().to_string();

        let ts_file_id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM ts_files WHERE path = ?")
                .bind(&path_str)
                .fetch_optional(&pool)
                .await?;

        let id = ts_file_id.ok_or_else(|| {
            anyhow::anyhow!(
                "path not found in DB: {}\n(run without --reingest first to ingest it)",
                path_str
            )
        })?;

        println!("[ingest_cli] reingest: {} (id={})", path_str, id);
        captu::ingest::reset_ts_file(&pool, id, &cache_dir).await?;
        captu::ingest::run_workers(config.clone(), pool.clone()).await?;

        println!("[ingest_cli] done.");
        return Ok(());
    }

    // Sub-command: --reingest-program <program_id>
    if let Some(pid_str) = args
        .windows(2)
        .find(|w| w[0] == "--reingest-program")
        .map(|w| &w[1])
    {
        let program_id: i64 = pid_str
            .parse()
            .map_err(|_| anyhow::anyhow!("--reingest-program requires a numeric program id"))?;

        let title: Option<String> =
            sqlx::query_scalar("SELECT title FROM programs WHERE id = ?")
                .bind(program_id)
                .fetch_optional(&pool)
                .await?;

        println!(
            "[ingest_cli] reingest program id={} ({})",
            program_id,
            title.as_deref().unwrap_or("?")
        );

        captu::ingest::reset_program(&pool, program_id, &cache_dir).await?;
        captu::ingest::run_workers(config.clone(), pool.clone()).await?;

        println!("[ingest_cli] done.");
        return Ok(());
    }

    // Default: scan + ingest
    captu::ingest::scan_and_ingest(config, pool.clone()).await?;

    // Print summary
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ts_files")
        .fetch_one(&pool)
        .await?;
    let done: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ts_files WHERE status = 'done'")
            .fetch_one(&pool)
            .await?;
    let error: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ts_files WHERE status = 'error'")
            .fetch_one(&pool)
            .await?;
    let caption_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captions")
        .fetch_one(&pool)
        .await?;

    println!("\n[ingest_cli] summary:");
    println!("  ts_files: total={} done={} error={}", total, done, error);
    println!("  captions: {}", caption_count);

    Ok(())
}
