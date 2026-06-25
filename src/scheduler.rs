use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::{config::Config, ingest};

/// Shared guard preventing overlapping scan runs.
/// Held by the startup scan in main.rs and by each scheduled tick.
/// `try_lock` in the scheduled job skips the tick if a scan is already in flight.
pub type IngestGuard = Arc<Mutex<()>>;

/// Build and start the periodic ingest scheduler.
///
/// Returns the running `JobScheduler` (caller must keep it alive — drop = stop).
/// Returns `Ok(None)` if `schedule_cron` is empty (scheduling disabled).
pub async fn start(
    config: Arc<Config>,
    pool: SqlitePool,
    guard: IngestGuard,
) -> Result<Option<JobScheduler>> {
    let cron = config.ingest.schedule_cron.trim().to_owned();
    if cron.is_empty() {
        tracing::info!("scheduler disabled (schedule_cron is empty)");
        return Ok(None);
    }

    let sched = JobScheduler::new().await?;

    let job = Job::new_async(cron.as_str(), move |_uuid, _l| {
        let config = config.clone();
        let pool = pool.clone();
        let guard = guard.clone();
        Box::pin(async move {
            // Skip this tick if a scan is still draining from the previous run.
            let _lock = match guard.try_lock() {
                Ok(l) => l,
                Err(_) => {
                    tracing::info!("scheduled ingest: previous scan still running, skipping tick");
                    return;
                }
            };
            tracing::info!("scheduled ingest: beginning scan");
            if let Err(e) = ingest::scan_and_ingest(config, pool).await {
                tracing::error!("scheduled ingest failed: {:#}", e);
            }
        })
    })?;

    sched.add(job).await?;
    sched.start().await?;
    tracing::info!("scheduler started (cron: {})", cron);
    Ok(Some(sched))
}
