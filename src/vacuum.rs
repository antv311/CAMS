//! Automated SQLite online defragmentation and non-blocking incremental vacuum worker.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use crate::db::{AutoVacuumMode, DbPageStats, SharedConnection};
use crate::history::unix_timestamp;
use crate::logging::{EventForwarder, Severity};

pub const DEFAULT_VACUUM_ENABLED: bool = true;
pub const DEFAULT_VACUUM_INTERVAL_SECS: u64 = 300;
pub const DEFAULT_VACUUM_MIN_FREELIST_PAGES: u64 = 64;
pub const DEFAULT_VACUUM_MIN_FREELIST_RATIO: f64 = 0.0;
pub const DEFAULT_VACUUM_BATCH_PAGES: u32 = 64;
pub const DEFAULT_VACUUM_STEP_DELAY_MS: u64 = 25;
pub const DEFAULT_VACUUM_MAX_BATCHES_PER_RUN: usize = 32;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VacuumConfig {
    /// Whether the periodic background incremental-vacuum worker is enabled.
    pub enabled: bool,
    /// Seconds between freelist checks (`0` disables the periodic timer).
    pub interval_secs: u64,
    /// Minimum SQLite freelist page count required before scheduled vacuum runs.
    pub min_freelist_pages: u64,
    /// Minimum freelist fraction (`0.0..=1.0`) of total pages required when > 0.0.
    pub min_freelist_ratio: f64,
    /// Maximum freelist pages reclaimed per `PRAGMA incremental_vacuum(N)` step.
    pub batch_pages: u32,
    /// Delay between consecutive vacuum steps so active writers never starve.
    pub step_delay_ms: u64,
    /// Cap on batches executed per scheduled pass (`0` means unlimited).
    pub max_batches_per_run: usize,
}

impl Default for VacuumConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_VACUUM_ENABLED,
            interval_secs: DEFAULT_VACUUM_INTERVAL_SECS,
            min_freelist_pages: DEFAULT_VACUUM_MIN_FREELIST_PAGES,
            min_freelist_ratio: DEFAULT_VACUUM_MIN_FREELIST_RATIO,
            batch_pages: DEFAULT_VACUUM_BATCH_PAGES,
            step_delay_ms: DEFAULT_VACUUM_STEP_DELAY_MS,
            max_batches_per_run: DEFAULT_VACUUM_MAX_BATCHES_PER_RUN,
        }
    }
}

impl VacuumConfig {
    pub fn validate(&self) -> Result<()> {
        if self.batch_pages == 0 {
            bail!("db_vacuum_batch_pages must be at least 1 (got 0)");
        }
        if !self.min_freelist_ratio.is_finite() || !(0.0..=1.0).contains(&self.min_freelist_ratio) {
            bail!(
                "db_vacuum_min_freelist_ratio must be between 0.0 and 1.0 (got {})",
                self.min_freelist_ratio
            );
        }
        Ok(())
    }

    pub fn should_vacuum(&self, stats: &DbPageStats) -> bool {
        if stats.freelist_count == 0 {
            return false;
        }
        if stats.freelist_count < self.min_freelist_pages {
            return false;
        }
        if self.min_freelist_ratio > 0.0 && stats.freelist_ratio < self.min_freelist_ratio {
            return false;
        }
        true
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct VacuumStatus {
    pub enabled: bool,
    pub interval_secs: u64,
    pub min_freelist_pages: u64,
    pub min_freelist_ratio: f64,
    pub batch_pages: u32,
    pub step_delay_ms: u64,
    pub max_batches_per_run: usize,
    pub running: bool,
    pub last_attempt_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_outcome: Option<String>,
    pub last_error: Option<String>,
    pub last_freelist_pages: u64,
    pub last_page_count: u64,
    pub last_freelist_ratio: f64,
    pub last_pages_freed: u64,
    pub last_bytes_freed: u64,
    pub total_runs: u64,
    pub total_pages_freed: u64,
    pub total_bytes_freed: u64,
    pub total_skipped_threshold: u64,
    pub total_skipped_busy: u64,
    pub total_failures: u64,
}

#[derive(Clone, Debug, Default)]
pub struct VacuumRunOptions {
    pub batch_pages: Option<u32>,
    pub max_pages: Option<u64>,
    pub blocking: bool,
    pub ignore_threshold: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct VacuumRunResult {
    pub trigger: String,
    pub outcome: String,
    pub auto_vacuum: AutoVacuumMode,
    pub freelist_before: u64,
    pub freelist_after: u64,
    pub page_count_before: u64,
    pub page_count_after: u64,
    pub page_size: u64,
    pub pages_freed: u64,
    pub bytes_freed: u64,
    pub batches_executed: usize,
    pub skipped_busy: bool,
    pub duration_ms: u64,
}

#[derive(Clone)]
pub struct VacuumManager {
    db: SharedConnection,
    config: Arc<RwLock<VacuumConfig>>,
    logger: EventForwarder,
    status: Arc<Mutex<VacuumStatus>>,
    op_lock: Arc<tokio::sync::Mutex<()>>,
    notify: Arc<Notify>,
    pending_check: Arc<AtomicBool>,
}

impl VacuumManager {
    pub fn new(db: SharedConnection, config: VacuumConfig, logger: EventForwarder) -> Self {
        let initial_status = VacuumStatus {
            enabled: config.enabled,
            interval_secs: config.interval_secs,
            min_freelist_pages: config.min_freelist_pages,
            min_freelist_ratio: config.min_freelist_ratio,
            batch_pages: config.batch_pages,
            step_delay_ms: config.step_delay_ms,
            max_batches_per_run: config.max_batches_per_run,
            ..VacuumStatus::default()
        };
        Self {
            db,
            config: Arc::new(RwLock::new(config)),
            logger,
            status: Arc::new(Mutex::new(initial_status)),
            op_lock: Arc::new(tokio::sync::Mutex::new(())),
            notify: Arc::new(Notify::new()),
            pending_check: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn config(&self) -> VacuumConfig {
        self.config.read().expect("vacuum config lock poisoned").clone()
    }

    pub fn update_config(&self, config: VacuumConfig) {
        {
            let mut guard = self.config.write().expect("vacuum config lock poisoned");
            *guard = config.clone();
        }
        {
            let mut status = self.status.lock().expect("vacuum status lock poisoned");
            status.enabled = config.enabled;
            status.interval_secs = config.interval_secs;
            status.min_freelist_pages = config.min_freelist_pages;
            status.min_freelist_ratio = config.min_freelist_ratio;
            status.batch_pages = config.batch_pages;
            status.step_delay_ms = config.step_delay_ms;
            status.max_batches_per_run = config.max_batches_per_run;
        }
        self.notify.notify_one();
    }

    pub fn status(&self) -> VacuumStatus {
        self.status
            .lock()
            .expect("vacuum status lock poisoned")
            .clone()
    }

    #[allow(dead_code)]
    pub fn trigger_check(&self) {
        self.pending_check.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    pub fn spawn_worker(&self) -> tokio::task::JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                let cfg = manager.config();
                if !cfg.enabled || cfg.interval_secs == 0 {
                    // Wait until config is updated or worker is woken.
                    manager.notify.notified().await;
                    if !manager.pending_check.swap(false, Ordering::AcqRel) {
                        continue;
                    }
                } else {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(cfg.interval_secs.max(1))) => {}
                        _ = manager.notify.notified() => {
                            if !manager.pending_check.swap(false, Ordering::AcqRel) {
                                continue;
                            }
                        }
                    }
                }

                let latest_cfg = manager.config();
                if !latest_cfg.enabled {
                    continue;
                }
                if let Err(err) = manager.run_once("scheduled").await {
                    manager.logger.log_error(
                        "db.vacuum_failed",
                        format!("scheduled incremental vacuum failed: {err:#}"),
                    );
                }
            }
        })
    }

    pub async fn run_once(&self, trigger: &str) -> Result<VacuumRunResult> {
        let opts = VacuumRunOptions {
            ignore_threshold: trigger != "scheduled",
            ..VacuumRunOptions::default()
        };
        self.run_with_options(trigger, opts).await
    }

    pub async fn run_with_options(
        &self,
        trigger: &str,
        opts: VacuumRunOptions,
    ) -> Result<VacuumRunResult> {
        let _op_guard = self.op_lock.lock().await;
        let started = Instant::now();
        let cfg = self.config();
        let batch_pages = opts.batch_pages.unwrap_or(cfg.batch_pages).max(1);
        let non_blocking = !opts.blocking;

        {
            let mut status = self.status.lock().expect("vacuum status lock poisoned");
            status.running = true;
            status.last_attempt_at = Some(unix_timestamp());
        }

        let outcome = self
            .execute_pass(trigger, &cfg, batch_pages, non_blocking, &opts, started)
            .await;

        match outcome {
            Ok(result) => {
                {
                    let mut status = self.status.lock().expect("vacuum status lock poisoned");
                    status.running = false;
                    status.last_run_at = Some(unix_timestamp());
                    status.last_outcome = Some(result.outcome.clone());
                    status.last_error = None;
                    status.last_freelist_pages = result.freelist_after;
                    status.last_page_count = result.page_count_after;
                    status.last_freelist_ratio = if result.page_count_after > 0 {
                        result.freelist_after as f64 / result.page_count_after as f64
                    } else {
                        0.0
                    };
                    status.last_pages_freed = result.pages_freed;
                    status.last_bytes_freed = result.bytes_freed;
                    status.total_runs += 1;
                    status.total_pages_freed += result.pages_freed;
                    status.total_bytes_freed += result.bytes_freed;
                    if result.outcome == "skipped_below_threshold" {
                        status.total_skipped_threshold += 1;
                    }
                    if result.skipped_busy {
                        status.total_skipped_busy += 1;
                    }
                }
                if result.pages_freed > 0 {
                    self.logger.log_security(
                        Severity::Info,
                        "db.vacuum_completed",
                        "success",
                        Some("system"),
                        None,
                        None,
                        format!(
                            "Incremental SQLite vacuum completed (trigger={}, outcome={}, pages_freed={}, bytes_freed={}, freelist_before={}, freelist_after={}, batches={})",
                            result.trigger,
                            result.outcome,
                            result.pages_freed,
                            result.bytes_freed,
                            result.freelist_before,
                            result.freelist_after,
                            result.batches_executed,
                        ),
                    );
                }
                Ok(result)
            }
            Err(err) => {
                let msg = format!("{err:#}");
                {
                    let mut status = self.status.lock().expect("vacuum status lock poisoned");
                    status.running = false;
                    status.last_run_at = Some(unix_timestamp());
                    status.last_outcome = Some("failed".to_string());
                    status.last_error = Some(msg.clone());
                    status.total_runs += 1;
                    status.total_failures += 1;
                }
                self.logger.log_error(
                    "db.vacuum_failed",
                    format!("SQLite incremental vacuum failed (trigger={trigger}): {msg}"),
                );
                Err(err)
            }
        }
    }

    async fn execute_pass(
        &self,
        trigger: &str,
        cfg: &VacuumConfig,
        batch_pages: u32,
        non_blocking: bool,
        opts: &VacuumRunOptions,
        started: Instant,
    ) -> Result<VacuumRunResult> {
        // Sample freelist via concurrent WAL reader pool (never blocks writers).
        let db_for_read = self.db.clone();
        let initial_stats = tokio::task::spawn_blocking(move || {
            if non_blocking {
                db_for_read.try_page_stats()
            } else {
                db_for_read.page_stats().map(Some)
            }
        })
        .await
        .context("join initial page_stats task")??;

        let Some(before) = initial_stats else {
            return Ok(VacuumRunResult {
                trigger: trigger.to_string(),
                outcome: "skipped_writer_busy".to_string(),
                auto_vacuum: AutoVacuumMode::Incremental,
                freelist_before: 0,
                freelist_after: 0,
                page_count_before: 0,
                page_count_after: 0,
                page_size: 0,
                pages_freed: 0,
                bytes_freed: 0,
                batches_executed: 0,
                skipped_busy: true,
                duration_ms: started.elapsed().as_millis() as u64,
            });
        };

        if before.auto_vacuum != AutoVacuumMode::Incremental {
            return Ok(VacuumRunResult {
                trigger: trigger.to_string(),
                outcome: "skipped_unsupported".to_string(),
                auto_vacuum: before.auto_vacuum,
                freelist_before: before.freelist_count,
                freelist_after: before.freelist_count,
                page_count_before: before.page_count,
                page_count_after: before.page_count,
                page_size: before.page_size,
                pages_freed: 0,
                bytes_freed: 0,
                batches_executed: 0,
                skipped_busy: false,
                duration_ms: started.elapsed().as_millis() as u64,
            });
        }

        if before.freelist_count == 0 {
            return Ok(VacuumRunResult {
                trigger: trigger.to_string(),
                outcome: "skipped_no_freelist".to_string(),
                auto_vacuum: before.auto_vacuum,
                freelist_before: 0,
                freelist_after: 0,
                page_count_before: before.page_count,
                page_count_after: before.page_count,
                page_size: before.page_size,
                pages_freed: 0,
                bytes_freed: 0,
                batches_executed: 0,
                skipped_busy: false,
                duration_ms: started.elapsed().as_millis() as u64,
            });
        }

        if !opts.ignore_threshold && !cfg.should_vacuum(&before) {
            return Ok(VacuumRunResult {
                trigger: trigger.to_string(),
                outcome: "skipped_below_threshold".to_string(),
                auto_vacuum: before.auto_vacuum,
                freelist_before: before.freelist_count,
                freelist_after: before.freelist_count,
                page_count_before: before.page_count,
                page_count_after: before.page_count,
                page_size: before.page_size,
                pages_freed: 0,
                bytes_freed: 0,
                batches_executed: 0,
                skipped_busy: false,
                duration_ms: started.elapsed().as_millis() as u64,
            });
        }

        let mut total_pages_freed: u64 = 0;
        let mut total_bytes_freed: u64 = 0;
        let mut batches_executed: usize = 0;
        let mut skipped_busy = false;
        let mut latest_freelist = before.freelist_count;

        loop {
            if let Some(cap) = opts.max_pages {
                if total_pages_freed >= cap {
                    break;
                }
            } else if cfg.max_batches_per_run > 0 && batches_executed >= cfg.max_batches_per_run {
                break;
            }

            let step_pages = match opts.max_pages {
                Some(cap) => {
                    let remaining = cap.saturating_sub(total_pages_freed);
                    batch_pages.min(remaining.min(u64::from(u32::MAX)) as u32).max(1)
                }
                None => batch_pages,
            };

            let db_step = self.db.clone();
            let step = tokio::task::spawn_blocking(move || {
                db_step.incremental_vacuum_step(step_pages, non_blocking)
            })
            .await
            .context("join incremental_vacuum_step task")??;

            if step.skipped_busy {
                skipped_busy = true;
                break;
            }

            batches_executed += 1;
            total_pages_freed += step.pages_freed;
            total_bytes_freed += step.bytes_freed;
            latest_freelist = step.freelist_after;

            if step.freelist_after == 0 || step.pages_freed == 0 {
                break;
            }

            if cfg.step_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(cfg.step_delay_ms)).await;
            } else {
                tokio::task::yield_now().await;
            }
        }

        let db_after = self.db.clone();
        let after = tokio::task::spawn_blocking(move || {
            if non_blocking {
                db_after.try_page_stats()
            } else {
                db_after.page_stats().map(Some)
            }
        })
        .await
        .context("join final page_stats task")??;

        let (freelist_after, page_count_after) = match after {
            Some(stats) => (stats.freelist_count, stats.page_count),
            None => (
                latest_freelist,
                before.page_count.saturating_sub(total_pages_freed),
            ),
        };

        let outcome = if skipped_busy && total_pages_freed > 0 {
            "partial_writer_busy"
        } else if skipped_busy {
            "skipped_writer_busy"
        } else if total_pages_freed > 0 {
            "completed"
        } else {
            "noop"
        };

        Ok(VacuumRunResult {
            trigger: trigger.to_string(),
            outcome: outcome.to_string(),
            auto_vacuum: before.auto_vacuum,
            freelist_before: before.freelist_count,
            freelist_after,
            page_count_before: before.page_count,
            page_count_after,
            page_size: before.page_size,
            pages_freed: total_pages_freed,
            bytes_freed: total_bytes_freed,
            batches_executed,
            skipped_busy,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SqlitePoolConfig;

    struct TempDb {
        dir: std::path::PathBuf,
        path: std::path::PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("aircd-vacuum-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("hub.sqlite3");
            Self { dir, path }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn seed_and_fragment(db: &SharedConnection, rows: usize) -> Result<DbPageStats> {
        {
            let writer = db.write()?;
            writer.execute_batch(
                "CREATE TABLE IF NOT EXISTS blobs (id INTEGER PRIMARY KEY, payload TEXT NOT NULL);",
            )?;
            writer.execute_batch("BEGIN IMMEDIATE;")?;
            let payload = "x".repeat(2048);
            for i in 0..rows {
                writer.execute(
                    "INSERT INTO blobs (id, payload) VALUES (?1, ?2)",
                    rusqlite::params![i as i64, &payload],
                )?;
            }
            writer.execute_batch("COMMIT;")?;
            writer.execute("DELETE FROM blobs WHERE id >= 10", [])?;
        }
        db.page_stats()
    }

    #[tokio::test]
    async fn scheduled_vacuum_respects_threshold_and_reclaims_freelist_in_batches() -> Result<()> {
        let tmp = TempDb::new("batches");
        let db = SharedConnection::open_pool(&tmp.path, SqlitePoolConfig::default(), |_| Ok(()))?;
        let stats = seed_and_fragment(&db, 200)?;
        assert_eq!(stats.auto_vacuum, AutoVacuumMode::Incremental);
        assert!(
            stats.freelist_count > 30,
            "expected fragmented freelist pages, got {}",
            stats.freelist_count
        );

        // Threshold higher than freelist skips without touching writer lock.
        let mgr = VacuumManager::new(
            db.clone(),
            VacuumConfig {
                enabled: true,
                interval_secs: 60,
                min_freelist_pages: stats.freelist_count + 100,
                min_freelist_ratio: 0.0,
                batch_pages: 16,
                step_delay_ms: 1,
                max_batches_per_run: 32,
            },
            EventForwarder::default(),
        );
        let skipped = mgr.run_once("scheduled").await?;
        assert_eq!(skipped.outcome, "skipped_below_threshold");
        assert_eq!(skipped.pages_freed, 0);

        // Lower threshold triggers multi-batch incremental vacuum and shrinks page_count.
        mgr.update_config(VacuumConfig {
            enabled: true,
            interval_secs: 60,
            min_freelist_pages: 10,
            min_freelist_ratio: 0.0,
            batch_pages: 16,
            step_delay_ms: 1,
            max_batches_per_run: 64,
        });
        let res = mgr.run_once("scheduled").await?;
        assert_eq!(res.outcome, "completed");
        assert!(res.batches_executed >= 2, "expected batched incremental vacuum");
        assert!(res.pages_freed > 0);
        assert_eq!(res.freelist_after, 0);
        assert!(res.page_count_after < res.page_count_before);

        let status = mgr.status();
        assert_eq!(status.total_pages_freed, res.pages_freed);
        assert_eq!(status.total_skipped_threshold, 1);
        assert_eq!(status.total_failures, 0);
        Ok(())
    }

    #[tokio::test]
    async fn vacuum_yields_immediately_when_writer_busy_and_never_blocks_readers() -> Result<()> {
        let tmp = TempDb::new("nonblocking");
        let db = SharedConnection::open_pool(
            &tmp.path,
            SqlitePoolConfig {
                read_pool_size: 4,
                ..Default::default()
            },
            |_| Ok(()),
        )?;
        let stats = seed_and_fragment(&db, 150)?;
        assert!(stats.freelist_count > 10);

        let mgr = VacuumManager::new(
            db.clone(),
            VacuumConfig {
                enabled: true,
                interval_secs: 60,
                min_freelist_pages: 5,
                min_freelist_ratio: 0.0,
                batch_pages: 16,
                step_delay_ms: 0,
                max_batches_per_run: 32,
            },
            EventForwarder::default(),
        );

        // Hold both a reader connection and the writer connection on the main thread
        // while triggering scheduled non-blocking vacuum.
        let reader = db.read()?;
        let row_count: i64 = reader.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?;
        assert_eq!(row_count, 10);

        let writer = db.write()?;
        let started = Instant::now();
        let res = mgr.run_once("scheduled").await?;
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "non-blocking vacuum should yield immediately when writer is held"
        );
        assert_eq!(res.outcome, "skipped_writer_busy");
        assert!(res.skipped_busy);
        assert_eq!(res.pages_freed, 0);
        assert_eq!(mgr.status().total_skipped_busy, 1);

        // Active reader still completes queries without locking or errors.
        let after_count: i64 = reader.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?;
        assert_eq!(after_count, 10);
        drop(reader);
        drop(writer);

        // Once writer is released, vacuum succeeds immediately.
        let res2 = mgr.run_once("scheduled").await?;
        assert_eq!(res2.outcome, "completed");
        assert!(res2.pages_freed > 0);
        Ok(())
    }

    #[tokio::test]
    async fn background_worker_runs_on_schedule_and_defers_to_active_readers_and_writers() -> Result<()> {
        let tmp = TempDb::new("worker");
        let db = SharedConnection::open_pool(&tmp.path, SqlitePoolConfig::default(), |_| Ok(()))?;
        let stats = seed_and_fragment(&db, 160)?;
        assert!(stats.freelist_count >= 20);

        let mgr = VacuumManager::new(
            db.clone(),
            VacuumConfig {
                enabled: true,
                interval_secs: 1,
                min_freelist_pages: 5,
                min_freelist_ratio: 0.0,
                batch_pages: 32,
                step_delay_ms: 2,
                max_batches_per_run: 32,
            },
            EventForwarder::default(),
        );
        let handle = mgr.spawn_worker();

        // Keep an active reader open while the worker vacuums in the background.
        let reader = db.read()?;
        let mut completed = false;
        for _ in 0..40 {
            if mgr.status().total_pages_freed > 0 {
                completed = true;
                break;
            }
            let cnt: i64 = reader.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0))?;
            assert_eq!(cnt, 10);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        handle.abort();
        assert!(completed, "background vacuum worker did not reclaim freelist pages");
        assert_eq!(db.page_stats()?.freelist_count, 0);
        Ok(())
    }
}
