//! Automated SQLite snapshot backup worker preserving database integrity and key rings.

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::history::unix_timestamp;
use crate::logging::{EventForwarder, Severity};
use crate::store::SharedConnection;

const SNAPSHOT_FORMAT_VERSION: u32 = 1;
const SNAPSHOT_PREFIX: &str = "snapshot-";
const STAGING_PREFIX: &str = ".staging-";
const DB_FILENAME: &str = "aircd.sqlite3";
const KEYRING_DIR: &str = "keyring";
const MANIFEST_FILENAME: &str = "manifest.json";
const DEFAULT_BACKUP_INTERVAL_SECS: u64 = 3600;
const DEFAULT_BACKUP_RETENTION: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupConfig {
    /// Whether the scheduled background backup worker is enabled.
    pub enabled: bool,
    /// Path to the live SQLite database (`:memory:` or disk path).
    pub db_path: String,
    /// Directory where snapshots are written. When `None` and `db_path != ":memory:"`,
    /// defaults to `<db_dir>/backups`. When `None` and `db_path == ":memory:"`,
    /// automated backups are skipped unless an explicit `backup_dir` is configured.
    pub backup_dir: Option<PathBuf>,
    /// Seconds between scheduled snapshots (`0` disables the periodic timer).
    pub interval_secs: u64,
    /// Maximum number of verified snapshots to keep (must be >= 1).
    pub retention: usize,
    /// Always capture a snapshot immediately on worker startup.
    pub on_startup: bool,
    /// Optional path to `aircd-owner.key` beside the DB.
    pub owner_key_path: Option<PathBuf>,
    /// Optional TLS certificate path.
    pub tls_cert_path: Option<PathBuf>,
    /// Optional TLS private key path.
    pub tls_key_path: Option<PathBuf>,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db_path: ":memory:".to_string(),
            backup_dir: None,
            interval_secs: DEFAULT_BACKUP_INTERVAL_SECS,
            retention: DEFAULT_BACKUP_RETENTION,
            on_startup: false,
            owner_key_path: None,
            tls_cert_path: None,
            tls_key_path: None,
        }
    }
}

impl BackupConfig {
    pub fn effective_backup_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.backup_dir {
            return Some(dir.clone());
        }
        if self.db_path == ":memory:" {
            return None;
        }
        Some(db_parent_dir(&self.db_path).join("backups"))
    }
}

pub fn db_parent_dir(db_path: &str) -> PathBuf {
    Path::new(db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

pub fn default_owner_key_path(db_path: &str) -> Option<PathBuf> {
    if db_path == ":memory:" {
        None
    } else {
        Some(db_parent_dir(db_path).join("aircd-owner.key"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRingFileEntry {
    pub kind: String,
    pub relative_path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRingSummary {
    pub principals: i64,
    pub tokens_total: i64,
    pub tokens_active: i64,
    pub tokens_revoked: i64,
    pub webhook_secrets: i64,
    pub git_webhook_secrets: i64,
    pub files: Vec<KeyRingFileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCounts {
    pub messages: i64,
    pub tasks: i64,
    pub projects: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub format_version: u32,
    pub snapshot_id: String,
    pub created_at: i64,
    pub trigger: String,
    pub db_file: String,
    pub db_bytes: u64,
    pub db_sha256: String,
    pub integrity_check: String,
    pub key_ring: KeyRingSummary,
    pub counts: SnapshotCounts,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotSummary {
    #[serde(flatten)]
    pub manifest: SnapshotManifest,
    pub path: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BackupStatus {
    pub enabled: bool,
    pub backup_dir: Option<String>,
    pub interval_secs: u64,
    pub retention: usize,
    pub on_startup: bool,
    pub running: bool,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub last_snapshot_id: Option<String>,
    pub last_error: Option<String>,
    pub consecutive_failures: u64,
    pub total_successes: u64,
    pub total_failures: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BackupRunResult {
    pub snapshot: SnapshotManifest,
    pub path: String,
    pub pruned: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotVerification {
    pub snapshot_id: String,
    pub path: String,
    pub ok: bool,
    pub integrity_check: String,
    pub db_sha256_verified: bool,
    pub key_ring_verified: bool,
    pub verified_files: usize,
    pub manifest: SnapshotManifest,
}

#[derive(Clone, Debug, Serialize)]
pub struct RestoreSummary {
    pub snapshot_id: String,
    pub restored_db: String,
    pub restored_key_files: Vec<String>,
    pub verification: SnapshotVerification,
}

#[derive(Clone)]
pub struct BackupManager {
    db: SharedConnection,
    config: Arc<RwLock<BackupConfig>>,
    logger: EventForwarder,
    status: Arc<Mutex<BackupStatus>>,
    op_lock: Arc<tokio::sync::Mutex<()>>,
    notify: Arc<Notify>,
    seq: Arc<AtomicU64>,
}

impl BackupManager {
    pub fn new(db: SharedConnection, config: BackupConfig, logger: EventForwarder) -> Self {
        let initial_status = BackupStatus {
            enabled: config.enabled,
            backup_dir: config
                .effective_backup_dir()
                .map(|p| p.display().to_string()),
            interval_secs: config.interval_secs,
            retention: config.retention.max(1),
            on_startup: config.on_startup,
            ..BackupStatus::default()
        };
        Self {
            db,
            config: Arc::new(RwLock::new(config)),
            logger,
            status: Arc::new(Mutex::new(initial_status)),
            op_lock: Arc::new(tokio::sync::Mutex::new(())),
            notify: Arc::new(Notify::new()),
            seq: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn config(&self) -> BackupConfig {
        self.config.read().expect("backup config lock poisoned").clone()
    }

    pub fn status(&self) -> BackupStatus {
        self.status.lock().expect("backup status lock poisoned").clone()
    }

    pub fn spawn_worker(&self) -> tokio::task::JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            let cfg = manager.config();
            if !cfg.enabled || cfg.effective_backup_dir().is_none() {
                return;
            }

            if let Some(dir) = cfg.effective_backup_dir() {
                let _guard = manager.op_lock.lock().await;
                let dir_clone = dir.clone();
                let _ = tokio::task::spawn_blocking(move || cleanup_stale_staging(&dir_clone)).await;
            }

            let should_run_startup = if cfg.on_startup {
                true
            } else {
                match manager.list_snapshots() {
                    Ok(existing) => match existing.first() {
                        None => true,
                        Some(latest) => {
                            cfg.interval_secs > 0
                                && unix_timestamp().saturating_sub(latest.manifest.created_at)
                                    >= cfg.interval_secs as i64
                        }
                    },
                    Err(_) => true,
                }
            };

            if should_run_startup {
                if let Err(err) = manager.run_once("startup").await {
                    manager.logger.log_error(
                        "backup.startup_failed",
                        format!("initial database snapshot backup failed: {err:#}"),
                    );
                }
            }

            loop {
                let cfg = manager.config();
                if !cfg.enabled || cfg.interval_secs == 0 || cfg.effective_backup_dir().is_none() {
                    break;
                }
                let wait_secs = manager.next_wait_secs(&cfg);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(wait_secs.max(1))) => {}
                    _ = manager.notify.notified() => {
                        continue;
                    }
                }
                let latest_cfg = manager.config();
                if !latest_cfg.enabled || latest_cfg.interval_secs == 0 {
                    break;
                }
                if let Err(err) = manager.run_once("scheduled").await {
                    manager.logger.log_error(
                        "backup.scheduled_failed",
                        format!("scheduled database snapshot backup failed: {err:#}"),
                    );
                }
            }
        })
    }

    fn next_wait_secs(&self, cfg: &BackupConfig) -> u64 {
        let interval = cfg.interval_secs.max(1);
        let Ok(existing) = self.list_snapshots() else {
            return interval;
        };
        let Some(latest) = existing.first() else {
            return interval;
        };
        let elapsed = unix_timestamp().saturating_sub(latest.manifest.created_at);
        if elapsed <= 0 {
            interval
        } else if (elapsed as u64) >= interval {
            1
        } else {
            interval - (elapsed as u64)
        }
    }

    pub async fn run_once(&self, trigger: &str) -> Result<BackupRunResult> {
        let _op_guard = self.op_lock.lock().await;
        let cfg = self.config();
        let backup_dir = cfg
            .effective_backup_dir()
            .ok_or_else(|| anyhow!("backup directory is unavailable for in-memory database"))?;

        {
            let mut status = self.status.lock().expect("backup status lock poisoned");
            status.running = true;
            status.last_attempt_at = Some(unix_timestamp());
        }

        let db = self.db.clone();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let trigger_owned = trigger.to_string();
        let backup_dir_clone = backup_dir.clone();
        let cfg_clone = cfg.clone();

        let result = tokio::task::spawn_blocking(move || {
            execute_snapshot_sync(&db, &cfg_clone, &backup_dir_clone, &trigger_owned, seq)
        })
        .await
        .context("join backup snapshot task")?;

        match result {
            Ok(run_result) => {
                {
                    let mut status = self.status.lock().expect("backup status lock poisoned");
                    status.running = false;
                    status.last_success_at = Some(run_result.snapshot.created_at);
                    status.last_snapshot_id = Some(run_result.snapshot.snapshot_id.clone());
                    status.last_error = None;
                    status.consecutive_failures = 0;
                    status.total_successes += 1;
                }
                let owner_key_preserved = run_result
                    .snapshot
                    .key_ring
                    .files
                    .iter()
                    .any(|f| f.kind == "owner_key");
                self.logger.log_security(
                    Severity::Info,
                    "backup.completed",
                    "success",
                    Some("system"),
                    None,
                    None,
                    format!(
                        "Database snapshot {} completed (trigger={}, db_bytes={}, tokens_active={}, owner_key_preserved={}, pruned={})",
                        run_result.snapshot.snapshot_id,
                        run_result.snapshot.trigger,
                        run_result.snapshot.db_bytes,
                        run_result.snapshot.key_ring.tokens_active,
                        owner_key_preserved,
                        run_result.pruned.len(),
                    ),
                );
                if !run_result.pruned.is_empty() {
                    self.logger.log_security(
                        Severity::Info,
                        "backup.pruned",
                        "success",
                        Some("system"),
                        None,
                        None,
                        format!(
                            "Pruned {} old database snapshot(s): {}",
                            run_result.pruned.len(),
                            run_result.pruned.join(", ")
                        ),
                    );
                }
                Ok(run_result)
            }
            Err(err) => {
                let err_msg = format!("{err:#}");
                {
                    let mut status = self.status.lock().expect("backup status lock poisoned");
                    status.running = false;
                    status.last_error = Some(err_msg.clone());
                    status.consecutive_failures += 1;
                    status.total_failures += 1;
                }
                self.logger.log_error(
                    "backup.failed",
                    format!("database snapshot backup failed (trigger={trigger}): {err_msg}"),
                );
                self.logger.log_security(
                    Severity::Warning,
                    "backup.failed",
                    "failure",
                    Some("system"),
                    None,
                    None,
                    format!("Database snapshot backup failed (trigger={trigger}): {err_msg}"),
                );
                Err(err)
            }
        }
    }

    pub fn list_snapshots(&self) -> Result<Vec<SnapshotSummary>> {
        let cfg = self.config();
        let Some(backup_dir) = cfg.effective_backup_dir() else {
            return Ok(Vec::new());
        };
        list_snapshots_in_dir(&backup_dir)
    }

    pub fn verify_snapshot_by_id(&self, snapshot_id: Option<&str>) -> Result<SnapshotVerification> {
        let cfg = self.config();
        let backup_dir = cfg
            .effective_backup_dir()
            .ok_or_else(|| anyhow!("backup directory is unavailable for in-memory database"))?;

        let target_dir = match snapshot_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => {
                validate_snapshot_id(id)?;
                backup_dir.join(id)
            }
            None => {
                let snapshots = list_snapshots_in_dir(&backup_dir)?;
                let latest = snapshots
                    .first()
                    .ok_or_else(|| anyhow!("no database snapshots found in {}", backup_dir.display()))?;
                PathBuf::from(&latest.path)
            }
        };

        verify_snapshot(&target_dir)
    }
}

fn execute_snapshot_sync(
    db: &SharedConnection,
    cfg: &BackupConfig,
    backup_dir: &Path,
    trigger: &str,
    seq: u64,
) -> Result<BackupRunResult> {
    std::fs::create_dir_all(backup_dir)
        .with_context(|| format!("create backup directory {}", backup_dir.display()))?;
    set_private_dir_perms(backup_dir)?;

    let now_system = SystemTime::now();
    let created_at = now_system
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let snapshot_id = format_snapshot_id(now_system, seq);

    let staging_dir = backup_dir.join(format!("{STAGING_PREFIX}{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&staging_dir)
        .with_context(|| format!("create staging dir {}", staging_dir.display()))?;
    set_private_dir_perms(&staging_dir)?;

    let staged_result = (|| -> Result<SnapshotManifest> {
        let staged_db = staging_dir.join(DB_FILENAME);
        let staged_db_str = staged_db
            .to_str()
            .ok_or_else(|| anyhow!("snapshot DB path is not valid UTF-8"))?;

        if cfg.db_path == ":memory:" {
            let conn = db.write()?;
            conn.execute("VACUUM INTO ?1", params![staged_db_str])
                .context("snapshot in-memory SQLite database via VACUUM INTO")?;
        } else {
            let source = Connection::open(&cfg.db_path).with_context(|| {
                format!("open source SQLite database for snapshot: {}", cfg.db_path)
            })?;
            source.busy_timeout(db.config().busy_timeout())?;
            source
                .execute("VACUUM INTO ?1", params![staged_db_str])
                .context("snapshot SQLite database via VACUUM INTO")?;
        }
        set_private_file_perms(&staged_db)?;

        let (integrity_check, mut key_ring, counts) = inspect_snapshot_db(&staged_db)?;
        if integrity_check != "ok" {
            bail!("snapshot SQLite integrity_check failed: {integrity_check}");
        }

        let db_meta = std::fs::metadata(&staged_db)
            .with_context(|| format!("stat staged snapshot DB {}", staged_db.display()))?;
        let db_sha256 = sha256_file(&staged_db)?;

        let keyring_dir = staging_dir.join(KEYRING_DIR);
        let mut key_files = Vec::new();
        copy_optional_key_file(
            cfg.owner_key_path.as_deref(),
            "owner_key",
            "aircd-owner.key",
            &keyring_dir,
            &mut key_files,
        )?;
        copy_optional_key_file(
            cfg.tls_cert_path.as_deref(),
            "tls_cert",
            "tls-cert.pem",
            &keyring_dir,
            &mut key_files,
        )?;
        copy_optional_key_file(
            cfg.tls_key_path.as_deref(),
            "tls_key",
            "tls-key.pem",
            &keyring_dir,
            &mut key_files,
        )?;
        key_ring.files = key_files;

        let manifest = SnapshotManifest {
            format_version: SNAPSHOT_FORMAT_VERSION,
            snapshot_id: snapshot_id.clone(),
            created_at,
            trigger: trigger.to_string(),
            db_file: DB_FILENAME.to_string(),
            db_bytes: db_meta.len(),
            db_sha256,
            integrity_check,
            key_ring,
            counts,
        };

        let manifest_path = staging_dir.join(MANIFEST_FILENAME);
        let manifest_json =
            serde_json::to_string_pretty(&manifest).context("serialize snapshot manifest")?;
        std::fs::write(&manifest_path, manifest_json)
            .with_context(|| format!("write snapshot manifest {}", manifest_path.display()))?;
        set_private_file_perms(&manifest_path)?;

        Ok(manifest)
    })();

    let manifest = match staged_result {
        Ok(manifest) => manifest,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(err);
        }
    };

    let final_dir = backup_dir.join(&snapshot_id);
    if let Err(err) = std::fs::rename(&staging_dir, &final_dir) {
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(err).with_context(|| {
            format!(
                "finalize snapshot directory {} -> {}",
                staging_dir.display(),
                final_dir.display()
            )
        });
    }

    let pruned = prune_snapshots(backup_dir, cfg.retention.max(1))?;

    Ok(BackupRunResult {
        snapshot: manifest,
        path: final_dir.display().to_string(),
        pruned,
    })
}

fn copy_optional_key_file(
    source_path: Option<&Path>,
    kind: &str,
    dest_name: &str,
    keyring_dir: &Path,
    out: &mut Vec<KeyRingFileEntry>,
) -> Result<()> {
    let Some(src) = source_path else {
        return Ok(());
    };
    if !src.is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(keyring_dir)
        .with_context(|| format!("create keyring backup dir {}", keyring_dir.display()))?;
    set_private_dir_perms(keyring_dir)?;

    let dest = keyring_dir.join(dest_name);
    copy_file_synced(src, &dest)
        .with_context(|| format!("copy key ring file {} -> {}", src.display(), dest.display()))?;
    set_private_file_perms(&dest)?;

    let bytes = std::fs::metadata(&dest)
        .with_context(|| format!("stat copied key file {}", dest.display()))?
        .len();
    let sha256 = sha256_file(&dest)?;
    out.push(KeyRingFileEntry {
        kind: kind.to_string(),
        relative_path: format!("{KEYRING_DIR}/{dest_name}"),
        bytes,
        sha256,
    });
    Ok(())
}

fn inspect_snapshot_db(db_path: &Path) -> Result<(String, KeyRingSummary, SnapshotCounts)> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open snapshot DB for verification: {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;

    let mut stmt = conn
        .prepare("PRAGMA integrity_check")
        .context("prepare PRAGMA integrity_check")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("read PRAGMA integrity_check result")?;
    let integrity = if rows.len() == 1 && rows[0].eq_ignore_ascii_case("ok") {
        "ok".to_string()
    } else if rows.is_empty() {
        "empty integrity_check result".to_string()
    } else {
        rows.join("; ")
    };

    let principals: i64 = conn.query_row("SELECT COUNT(*) FROM principals", [], |r| r.get(0))?;
    let tokens_total: i64 =
        conn.query_row("SELECT COUNT(*) FROM principal_tokens", [], |r| r.get(0))?;
    let tokens_active: i64 = conn.query_row(
        "SELECT COUNT(*) FROM principal_tokens WHERE revoked_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    let tokens_revoked = tokens_total.saturating_sub(tokens_active);
    let webhook_secrets: i64 = conn.query_row(
        "SELECT COUNT(*) FROM webhooks WHERE secret IS NOT NULL AND secret != ''",
        [],
        |r| r.get(0),
    )?;
    let git_webhook_secrets: i64 = conn.query_row(
        "SELECT COUNT(*) FROM git_repos WHERE webhook_secret IS NOT NULL AND webhook_secret != ''",
        [],
        |r| r.get(0),
    )?;

    let messages: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let tasks: i64 = conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))?;
    let projects: i64 = conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))?;

    Ok((
        integrity,
        KeyRingSummary {
            principals,
            tokens_total,
            tokens_active,
            tokens_revoked,
            webhook_secrets,
            git_webhook_secrets,
            files: Vec::new(),
        },
        SnapshotCounts {
            messages,
            tasks,
            projects,
        },
    ))
}

pub fn list_snapshots_in_dir(backup_dir: &Path) -> Result<Vec<SnapshotSummary>> {
    if !backup_dir.exists() {
        return Ok(Vec::new());
    }
    let mut snapshots = Vec::new();
    for entry in std::fs::read_dir(backup_dir)
        .with_context(|| format!("read backup directory {}", backup_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(SNAPSHOT_PREFIX) {
            continue;
        }
        let manifest_path = path.join(MANIFEST_FILENAME);
        if !manifest_path.is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("read manifest {}", manifest_path.display()))?;
        let manifest: SnapshotManifest = serde_json::from_str(&raw)
            .with_context(|| format!("parse manifest {}", manifest_path.display()))?;
        snapshots.push(SnapshotSummary {
            manifest,
            path: path.display().to_string(),
        });
    }
    snapshots.sort_by(|a, b| {
        b.manifest
            .created_at
            .cmp(&a.manifest.created_at)
            .then_with(|| b.manifest.snapshot_id.cmp(&a.manifest.snapshot_id))
    });
    Ok(snapshots)
}

pub fn verify_snapshot(snapshot_dir: &Path) -> Result<SnapshotVerification> {
    let manifest_path = snapshot_dir.join(MANIFEST_FILENAME);
    let raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read snapshot manifest {}", manifest_path.display()))?;
    let manifest: SnapshotManifest = serde_json::from_str(&raw)
        .with_context(|| format!("parse snapshot manifest {}", manifest_path.display()))?;

    if manifest.format_version != SNAPSHOT_FORMAT_VERSION {
        bail!(
            "unsupported snapshot format_version {}",
            manifest.format_version
        );
    }
    validate_snapshot_id(&manifest.snapshot_id)?;
    if manifest.db_file != DB_FILENAME {
        bail!("unexpected snapshot db_file '{}'", manifest.db_file);
    }

    let db_path = snapshot_dir.join(&manifest.db_file);
    if !db_path.is_file() {
        bail!("snapshot database file missing: {}", db_path.display());
    }
    let actual_bytes = std::fs::metadata(&db_path)?.len();
    if actual_bytes != manifest.db_bytes {
        bail!(
            "snapshot database size mismatch: expected {}, got {}",
            manifest.db_bytes,
            actual_bytes
        );
    }
    let actual_sha = sha256_file(&db_path)?;
    if actual_sha != manifest.db_sha256 {
        bail!("snapshot database SHA-256 mismatch");
    }

    let (integrity_check, observed_ring, _counts) = inspect_snapshot_db(&db_path)?;
    if integrity_check != "ok" {
        bail!("snapshot SQLite integrity_check failed: {integrity_check}");
    }
    if observed_ring.principals != manifest.key_ring.principals
        || observed_ring.tokens_total != manifest.key_ring.tokens_total
        || observed_ring.tokens_active != manifest.key_ring.tokens_active
        || observed_ring.webhook_secrets != manifest.key_ring.webhook_secrets
        || observed_ring.git_webhook_secrets != manifest.key_ring.git_webhook_secrets
    {
        bail!("snapshot database key ring counts do not match manifest");
    }

    let mut verified_files = 0usize;
    for file in &manifest.key_ring.files {
        let rel = validate_keyring_rel_path(&file.relative_path)?;
        let full = snapshot_dir.join(rel);
        if !full.is_file() {
            bail!("snapshot key ring file missing: {}", full.display());
        }
        let len = std::fs::metadata(&full)?.len();
        if len != file.bytes {
            bail!(
                "snapshot key ring file {} size mismatch: expected {}, got {}",
                file.relative_path,
                file.bytes,
                len
            );
        }
        let digest = sha256_file(&full)?;
        if digest != file.sha256 {
            bail!(
                "snapshot key ring file {} SHA-256 mismatch",
                file.relative_path
            );
        }
        verified_files += 1;
    }

    Ok(SnapshotVerification {
        snapshot_id: manifest.snapshot_id.clone(),
        path: snapshot_dir.display().to_string(),
        ok: true,
        integrity_check,
        db_sha256_verified: true,
        key_ring_verified: true,
        verified_files,
        manifest,
    })
}

#[allow(dead_code)]
pub fn restore_snapshot(
    snapshot_dir: &Path,
    target_db: &Path,
    target_owner_key: Option<&Path>,
    target_tls_cert: Option<&Path>,
    target_tls_key: Option<&Path>,
) -> Result<RestoreSummary> {
    let verification = verify_snapshot(snapshot_dir)?;
    let manifest = &verification.manifest;

    let src_db = snapshot_dir.join(&manifest.db_file);
    atomic_copy_replace(&src_db, target_db, true)?;

    // Remove stale WAL/SHM sidecars beside the restored standalone DB so old frames
    // cannot be replayed on top of the restored snapshot.
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", target_db.display(), suffix));
        if sidecar.exists() {
            let _ = std::fs::remove_file(&sidecar);
        }
    }

    let mut restored_key_files = Vec::new();
    for entry in &manifest.key_ring.files {
        let rel = validate_keyring_rel_path(&entry.relative_path)?;
        let src_file = snapshot_dir.join(rel);
        let dest = match entry.kind.as_str() {
            "owner_key" => target_owner_key,
            "tls_cert" => target_tls_cert,
            "tls_key" => target_tls_key,
            _ => None,
        };
        if let Some(dest_path) = dest {
            let private = entry.kind != "tls_cert";
            atomic_copy_replace(&src_file, dest_path, private)?;
            restored_key_files.push(dest_path.display().to_string());
        }
    }

    Ok(RestoreSummary {
        snapshot_id: manifest.snapshot_id.clone(),
        restored_db: target_db.display().to_string(),
        restored_key_files,
        verification,
    })
}

fn atomic_copy_replace(src: &Path, dest: &Path, private_perms: bool) -> Result<()> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create target directory {}", parent.display()))?;
    let temp = parent.join(format!(".restore-{}.tmp", Uuid::new_v4().simple()));
    if let Err(err) = (|| -> Result<()> {
        copy_file_synced(src, &temp)
            .with_context(|| format!("copy {} -> {}", src.display(), temp.display()))?;
        if private_perms {
            set_private_file_perms(&temp)?;
        }
        std::fs::rename(&temp, dest)
            .with_context(|| format!("replace {} -> {}", temp.display(), dest.display()))?;
        Ok(())
    })() {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

fn prune_snapshots(backup_dir: &Path, retention: usize) -> Result<Vec<String>> {
    let snapshots = list_snapshots_in_dir(backup_dir)?;
    let mut pruned = Vec::new();
    for old in snapshots.into_iter().skip(retention.max(1)) {
        let path = PathBuf::from(&old.path);
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("prune old snapshot {}", path.display()))?;
        pruned.push(old.manifest.snapshot_id);
    }
    Ok(pruned)
}

fn cleanup_stale_staging(backup_dir: &Path) -> Result<()> {
    if !backup_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(backup_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with(STAGING_PREFIX) {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    Ok(())
}

fn validate_snapshot_id(id: &str) -> Result<()> {
    if !id.starts_with(SNAPSHOT_PREFIX)
        || id.len() <= SNAPSHOT_PREFIX.len()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid snapshot_id '{id}'");
    }
    Ok(())
}

fn validate_keyring_rel_path(rel: &str) -> Result<PathBuf> {
    let path = Path::new(rel);
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        bail!("invalid key ring path '{rel}'");
    };
    let Some(Component::Normal(second)) = components.next() else {
        bail!("invalid key ring path '{rel}'");
    };
    if components.next().is_some() || first != KEYRING_DIR || second.is_empty() {
        bail!("invalid key ring path '{rel}'");
    }
    Ok(PathBuf::from(first).join(second))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {} for hashing", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

fn copy_file_synced(src: &Path, dest: &Path) -> Result<()> {
    let mut reader =
        File::open(src).with_context(|| format!("open source file {}", src.display()))?;
    let mut writer = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)
        .with_context(|| format!("create destination file {}", dest.display()))?;
    std::io::copy(&mut reader, &mut writer)
        .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
    writer
        .sync_all()
        .with_context(|| format!("sync {}", dest.display()))
}

#[cfg(unix)]
fn set_private_dir_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set 0700 permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_dir_perms(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set 0600 permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_perms(_path: &Path) -> Result<()> {
    Ok(())
}

fn format_snapshot_id(now: SystemTime, seq: u64) -> String {
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let (year, month, day, hour, minute, second) = utc_ymdhms(secs);
    format!(
        "{SNAPSHOT_PREFIX}{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z-{millis:03}-{seq:04}"
    )
}

fn utc_ymdhms(unix_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400) as u32;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m, d, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthStore;
    use crate::gits::GitRepoStore;
    use crate::history::HistoryStore;
    use crate::store;
    use crate::webhooks::WebhookStore;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn utc_ymdhms_formats_epoch_and_modern_dates() {
        assert_eq!(utc_ymdhms(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_ymdhms(1_700_000_000), (2023, 11, 14, 22, 13, 20));
    }

    #[tokio::test]
    async fn snapshot_preserves_db_integrity_tokens_and_owner_key_and_restores() -> Result<()> {
        let dir = TempDir::new("aircd-backup-roundtrip");
        let db_path = dir.0.join("hub.sqlite3");
        let owner_key_path = dir.0.join("aircd-owner.key");
        let backup_dir = dir.0.join("backups");
        let tls_cert = dir.0.join("cert.pem");
        let tls_key = dir.0.join("key.pem");

        std::fs::write(&owner_key_path, "owner-secret-fixture-key\n")?;
        std::fs::write(&tls_cert, "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n")?;
        std::fs::write(&tls_key, "-----BEGIN PRIVATE KEY-----\ntest\n-----END PRIVATE KEY-----\n")?;

        let db = store::open(&db_path)?;
        let auth = AuthStore::new(db.clone());
        let history = HistoryStore::new(db.clone());
        let webhooks = WebhookStore::new(db.clone());
        let gits = GitRepoStore::new(db.clone());

        let (alice, alice_token) = auth.register_principal("alice", Some("dev"), None)?;
        let bob_token_reissued = auth.reissue_all(&alice.id)?;
        assert!(auth.authenticate(&alice_token).is_err());
        assert_eq!(auth.authenticate(&bob_token_reissued)?.id, alice.id);

        webhooks.upsert(&alice.id, "http://127.0.0.1:9999/hook", None, None, false)?;
        gits.upsert("repo-a", "#cams", dir.0.to_str().unwrap(), None, Some("main"), None)?;
        history.append("#general", &alice.id, "hello before snapshot")?;

        let cfg = BackupConfig {
            enabled: true,
            db_path: db_path.display().to_string(),
            backup_dir: Some(backup_dir.clone()),
            interval_secs: 3600,
            retention: 5,
            on_startup: false,
            owner_key_path: Some(owner_key_path.clone()),
            tls_cert_path: Some(tls_cert.clone()),
            tls_key_path: Some(tls_key.clone()),
        };
        let manager = BackupManager::new(db.clone(), cfg, EventForwarder::default());
        let run = manager.run_once("manual").await?;

        assert_eq!(run.snapshot.integrity_check, "ok");
        assert!(run.snapshot.key_ring.principals >= 1);
        assert!(run.snapshot.key_ring.tokens_active >= 1);
        assert!(run.snapshot.key_ring.tokens_revoked >= 1);
        assert_eq!(run.snapshot.key_ring.webhook_secrets, 1);
        assert_eq!(run.snapshot.key_ring.git_webhook_secrets, 1);
        assert_eq!(run.snapshot.key_ring.files.len(), 3);
        assert!(run.snapshot.counts.messages >= 1);

        let verification = verify_snapshot(Path::new(&run.path))?;
        assert!(verification.ok);
        assert!(verification.db_sha256_verified);
        assert!(verification.key_ring_verified);
        assert_eq!(verification.verified_files, 3);

        // Mutate live DB and delete owner key, then restore from snapshot.
        let (charlie, charlie_token) = auth.register_principal("charlie", None, None)?;
        assert_eq!(auth.authenticate(&charlie_token)?.id, charlie.id);
        drop(auth);
        drop(history);
        drop(webhooks);
        drop(gits);
        drop(manager);
        drop(db);

        std::fs::remove_file(&owner_key_path)?;
        std::fs::write(&db_path, b"corrupted")?;

        let restored = restore_snapshot(
            Path::new(&run.path),
            &db_path,
            Some(&owner_key_path),
            Some(&tls_cert),
            Some(&tls_key),
        )?;
        assert_eq!(restored.snapshot_id, run.snapshot.snapshot_id);
        assert_eq!(restored.restored_key_files.len(), 3);
        assert_eq!(
            std::fs::read_to_string(&owner_key_path)?,
            "owner-secret-fixture-key\n"
        );

        let reopened = store::open(&db_path)?;
        let auth_after = AuthStore::new(reopened.clone());
        assert_eq!(auth_after.authenticate(&bob_token_reissued)?.id, alice.id);
        assert!(auth_after.authenticate(&alice_token).is_err());
        assert!(auth_after.authenticate(&charlie_token).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn retention_prunes_oldest_snapshots_and_detects_tampering() -> Result<()> {
        let dir = TempDir::new("aircd-backup-retention");
        let db_path = dir.0.join("hub.sqlite3");
        let owner_key_path = dir.0.join("aircd-owner.key");
        let backup_dir = dir.0.join("backups");
        std::fs::write(&owner_key_path, "owner-key-v1\n")?;

        let db = store::open(&db_path)?;
        let cfg = BackupConfig {
            enabled: true,
            db_path: db_path.display().to_string(),
            backup_dir: Some(backup_dir.clone()),
            interval_secs: 3600,
            retention: 2,
            on_startup: false,
            owner_key_path: Some(owner_key_path),
            tls_cert_path: None,
            tls_key_path: None,
        };
        let manager = BackupManager::new(db, cfg, EventForwarder::default());

        let r1 = manager.run_once("scheduled").await?;
        let r2 = manager.run_once("scheduled").await?;
        let r3 = manager.run_once("scheduled").await?;

        assert_eq!(r3.pruned, vec![r1.snapshot.snapshot_id.clone()]);
        let listed = manager.list_snapshots()?;
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].manifest.snapshot_id, r3.snapshot.snapshot_id);
        assert_eq!(listed[1].manifest.snapshot_id, r2.snapshot.snapshot_id);

        // Tampering with the preserved owner key fails verification and refuses restore.
        let key_copy = Path::new(&r3.path).join("keyring").join("aircd-owner.key");
        std::fs::write(&key_copy, "tampered-key\n")?;
        assert!(verify_snapshot(Path::new(&r3.path)).is_err());
        assert!(
            restore_snapshot(Path::new(&r3.path), &dir.0.join("out.sqlite3"), None, None, None)
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn scheduled_worker_runs_and_tolerates_concurrent_writes() -> Result<()> {
        let dir = TempDir::new("aircd-backup-worker");
        let db_path = dir.0.join("hub.sqlite3");
        let backup_dir = dir.0.join("backups");
        let db = store::open(&db_path)?;
        let history = HistoryStore::new(db.clone());

        let cfg = BackupConfig {
            enabled: true,
            db_path: db_path.display().to_string(),
            backup_dir: Some(backup_dir.clone()),
            interval_secs: 1,
            retention: 3,
            on_startup: true,
            owner_key_path: None,
            tls_cert_path: None,
            tls_key_path: None,
        };
        let manager = BackupManager::new(db, cfg, EventForwarder::default());
        let handle = manager.spawn_worker();

        for i in 0..10 {
            history.append("#general", "system", &format!("msg-{i}"))?;
            tokio::time::sleep(Duration::from_millis(130)).await;
        }
        handle.abort();

        let status = manager.status();
        assert!(status.total_successes >= 1);
        assert_eq!(status.total_failures, 0);
        let listed = manager.list_snapshots()?;
        assert!(!listed.is_empty());
        assert!(verify_snapshot(Path::new(&listed[0].path))?.ok);
        Ok(())
    }
}
