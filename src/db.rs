//! SQLite connection pooling, busy-timeout handling, and WAL tuning.

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub const DEFAULT_DB_POOL_SIZE: usize = 8;
pub const MAX_DB_POOL_SIZE: usize = 128;
pub const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5000;
pub const DEFAULT_WAL_AUTOCHECKPOINT: u32 = 1000;
pub const DEFAULT_CACHE_SIZE_KIB: i64 = 16_384; // 16 MiB per connection
pub const DEFAULT_JOURNAL_SIZE_LIMIT_BYTES: i64 = 67_108_864; // 64 MiB

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SqliteSynchronous {
    Off,
    #[default]
    Normal,
    Full,
    Extra,
}

impl SqliteSynchronous {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => Ok(Self::Off),
            "normal" | "1" => Ok(Self::Normal),
            "full" | "2" => Ok(Self::Full),
            "extra" | "3" => Ok(Self::Extra),
            other => bail!(
                "db_synchronous must be one of off, normal, full, extra (got '{other}')"
            ),
        }
    }

    pub fn as_pragma_str(&self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
            Self::Extra => "EXTRA",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Normal => "normal",
            Self::Full => "full",
            Self::Extra => "extra",
        }
    }
}

impl fmt::Display for SqliteSynchronous {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for SqliteSynchronous {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WalCheckpointMode {
    #[default]
    Passive,
    Full,
    Restart,
    Truncate,
}

impl WalCheckpointMode {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "passive" => Ok(Self::Passive),
            "full" => Ok(Self::Full),
            "restart" => Ok(Self::Restart),
            "truncate" => Ok(Self::Truncate),
            other => bail!(
                "checkpoint mode must be one of passive, full, restart, truncate (got '{other}')"
            ),
        }
    }

    pub fn as_pragma_str(&self) -> &'static str {
        match self {
            Self::Passive => "PASSIVE",
            Self::Full => "FULL",
            Self::Restart => "RESTART",
            Self::Truncate => "TRUNCATE",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SqlitePoolConfig {
    /// Maximum number of concurrent read-only SQLite connections for file-backed DBs.
    pub read_pool_size: usize,
    /// SQLite busy_timeout in milliseconds applied to every connection.
    pub busy_timeout_ms: u64,
    /// SQLite PRAGMA synchronous setting for WAL writes.
    pub synchronous: SqliteSynchronous,
    /// SQLite PRAGMA wal_autocheckpoint threshold in pages (0 disables).
    pub wal_autocheckpoint: u32,
    /// Per-connection page cache size in KiB (applied as PRAGMA cache_size = -KiB; 0 uses default).
    pub cache_size_kib: i64,
    /// SQLite PRAGMA journal_size_limit in bytes (-1 means unlimited).
    pub journal_size_limit_bytes: i64,
}

impl Default for SqlitePoolConfig {
    fn default() -> Self {
        Self {
            read_pool_size: DEFAULT_DB_POOL_SIZE,
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            synchronous: SqliteSynchronous::Normal,
            wal_autocheckpoint: DEFAULT_WAL_AUTOCHECKPOINT,
            cache_size_kib: DEFAULT_CACHE_SIZE_KIB,
            journal_size_limit_bytes: DEFAULT_JOURNAL_SIZE_LIMIT_BYTES,
        }
    }
}

impl SqlitePoolConfig {
    pub fn validate(&self) -> Result<()> {
        if self.read_pool_size == 0 || self.read_pool_size > MAX_DB_POOL_SIZE {
            bail!(
                "db_pool_size must be between 1 and {MAX_DB_POOL_SIZE} (got {})",
                self.read_pool_size
            );
        }
        if self.cache_size_kib < 0 {
            bail!(
                "db_cache_size_kib must be non-negative (got {})",
                self.cache_size_kib
            );
        }
        if self.journal_size_limit_bytes < -1 {
            bail!(
                "db_journal_size_limit must be >= -1 (got {})",
                self.journal_size_limit_bytes
            );
        }
        Ok(())
    }

    pub fn busy_timeout(&self) -> Duration {
        Duration::from_millis(self.busy_timeout_ms)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WalCheckpointResult {
    pub mode: String,
    pub busy: i64,
    pub log_frames: i64,
    pub checkpointed_frames: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PoolStats {
    pub is_in_memory: bool,
    pub read_pool_max: usize,
    pub read_pool_created: usize,
    pub read_pool_idle: usize,
    pub read_pool_in_use: usize,
    pub read_acquires: u64,
    pub write_acquires: u64,
    pub read_waits: u64,
    pub read_wait_ms: u64,
    pub write_wait_ms: u64,
    pub checkpoints: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LivePragmas {
    pub journal_mode: String,
    pub synchronous: String,
    pub busy_timeout_ms: i64,
    pub wal_autocheckpoint: i64,
    pub cache_size: i64,
    pub journal_size_limit: i64,
    pub temp_store: String,
    pub reader_query_only: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WalFileStats {
    pub db_path: String,
    pub db_bytes: Option<u64>,
    pub wal_bytes: Option<u64>,
    pub shm_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SqliteDiagnostics {
    pub config: SqlitePoolConfig,
    pub pool: PoolStats,
    pub pragmas: LivePragmas,
    pub files: Option<WalFileStats>,
}

#[derive(Default)]
struct PoolMetrics {
    read_acquires: AtomicU64,
    write_acquires: AtomicU64,
    read_waits: AtomicU64,
    read_wait_ns: AtomicU64,
    write_wait_ns: AtomicU64,
    checkpoints: AtomicU64,
}

struct ReaderPoolState {
    idle: Vec<Connection>,
    created: usize,
    max_size: usize,
}

struct ReaderPool {
    path: PathBuf,
    config: SqlitePoolConfig,
    state: Mutex<ReaderPoolState>,
    available: Condvar,
}

impl ReaderPool {
    fn new(path: PathBuf, config: SqlitePoolConfig) -> Result<Self> {
        let max_size = config.read_pool_size;
        let first = open_reader_connection(&path, &config)?;
        Ok(Self {
            path,
            config,
            state: Mutex::new(ReaderPoolState {
                idle: vec![first],
                created: 1,
                max_size,
            }),
            available: Condvar::new(),
        })
    }

    fn acquire(&self, metrics: &PoolMetrics) -> Result<Connection> {
        let mut state = self
            .state
            .lock()
            .expect("sqlite reader pool mutex poisoned");
        let mut counted_wait = false;

        loop {
            if let Some(conn) = state.idle.pop() {
                return Ok(conn);
            }
            if state.created < state.max_size {
                state.created += 1;
                drop(state);
                match open_reader_connection(&self.path, &self.config) {
                    Ok(conn) => return Ok(conn),
                    Err(err) => {
                        let mut state = self
                            .state
                            .lock()
                            .expect("sqlite reader pool mutex poisoned");
                        state.created = state.created.saturating_sub(1);
                        self.available.notify_one();
                        return Err(err);
                    }
                }
            }
            if !counted_wait {
                metrics.read_waits.fetch_add(1, Ordering::Relaxed);
                counted_wait = true;
            }
            state = self
                .available
                .wait(state)
                .expect("sqlite reader pool mutex poisoned");
        }
    }

    fn release(&self, conn: Connection) {
        if !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err() {
            let mut state = self
                .state
                .lock()
                .expect("sqlite reader pool mutex poisoned");
            state.created = state.created.saturating_sub(1);
            self.available.notify_one();
            return;
        }
        let mut state = self
            .state
            .lock()
            .expect("sqlite reader pool mutex poisoned");
        if state.idle.len() < state.max_size {
            state.idle.push(conn);
        } else {
            state.created = state.created.saturating_sub(1);
        }
        self.available.notify_one();
    }

    fn clear_idle(&self) {
        let mut state = self
            .state
            .lock()
            .expect("sqlite reader pool mutex poisoned");
        let removed = state.idle.len();
        state.idle.clear();
        state.created = state.created.saturating_sub(removed);
    }

    fn counts(&self) -> (usize, usize, usize, usize) {
        let state = self
            .state
            .lock()
            .expect("sqlite reader pool mutex poisoned");
        let idle = state.idle.len();
        let created = state.created;
        let in_use = created.saturating_sub(idle);
        (state.max_size, created, idle, in_use)
    }
}

struct PoolInner {
    path: Option<PathBuf>,
    config: SqlitePoolConfig,
    writer: Mutex<Connection>,
    readers: Option<ReaderPool>,
    metrics: PoolMetrics,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        if let Some(readers) = &self.readers {
            readers.clear_idle();
        }
        if self.path.is_some() {
            if let Ok(writer) = self.writer.lock() {
                let _ = writer.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
            }
        }
    }
}

/// Shared handle to the hub's SQLite database pool.
///
/// File-backed databases use a dedicated serialized writer connection plus a
/// bounded pool of concurrent `SQLITE_OPEN_READ_ONLY` WAL reader connections.
/// In-memory databases route both reads and writes through the single shared
/// connection so every caller sees the same memory database.
#[derive(Clone)]
pub struct SharedConnection {
    inner: Arc<PoolInner>,
}

pub struct PooledConnection<'a>(PooledConnectionInner<'a>);

enum PooledConnectionInner<'a> {
    Writer {
        guard: MutexGuard<'a, Connection>,
    },
    Reader {
        conn: Option<Connection>,
        pool: &'a ReaderPool,
    },
}

impl Deref for PooledConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            PooledConnectionInner::Writer { guard } => guard,
            PooledConnectionInner::Reader { conn, .. } => {
                conn.as_ref().expect("reader connection missing")
            }
        }
    }
}

impl DerefMut for PooledConnection<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.0 {
            PooledConnectionInner::Writer { guard } => guard,
            PooledConnectionInner::Reader { conn, .. } => {
                conn.as_mut().expect("reader connection missing")
            }
        }
    }
}

impl Drop for PooledConnection<'_> {
    fn drop(&mut self) {
        if let PooledConnectionInner::Reader { conn, pool } = &mut self.0 {
            if let Some(conn) = conn.take() {
                pool.release(conn);
            }
        }
    }
}

impl SharedConnection {
    pub fn open_pool(
        path: impl AsRef<Path>,
        config: SqlitePoolConfig,
        init: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<Self> {
        config.validate()?;
        let path_ref = path.as_ref();
        let is_memory = path_ref == Path::new(":memory:");
        let writer = Connection::open(path_ref).context("open sqlite database")?;
        apply_writer_pragmas(&writer, &config, !is_memory)?;
        init(&writer)?;

        let (stored_path, readers) = if is_memory {
            (None, None)
        } else {
            let owned = path_ref.to_path_buf();
            let pool = ReaderPool::new(owned.clone(), config.clone())
                .context("initialize sqlite reader pool")?;
            (Some(owned), Some(pool))
        };

        Ok(Self {
            inner: Arc::new(PoolInner {
                path: stored_path,
                config,
                writer: Mutex::new(writer),
                readers,
                metrics: PoolMetrics::default(),
            }),
        })
    }

    /// Wrap an already-opened connection (typically `:memory:` in unit tests).
    #[cfg(test)]
    pub fn from_connection(conn: Connection) -> Self {
        let config = SqlitePoolConfig::default();
        let _ = apply_common_pragmas(&conn, &config);
        Self {
            inner: Arc::new(PoolInner {
                path: None,
                config,
                writer: Mutex::new(conn),
                readers: None,
                metrics: PoolMetrics::default(),
            }),
        }
    }

    /// Acquire a connection for read-only queries. Uses the concurrent WAL
    /// reader pool for file-backed databases and falls back to the single
    /// connection for `:memory:` databases.
    pub fn read(&self) -> Result<PooledConnection<'_>> {
        let started = Instant::now();
        let out = match &self.inner.readers {
            Some(pool) => {
                let conn = pool.acquire(&self.inner.metrics)?;
                PooledConnection(PooledConnectionInner::Reader {
                    conn: Some(conn),
                    pool,
                })
            }
            None => {
                let guard = self.inner.writer.lock().expect("sqlite mutex poisoned");
                PooledConnection(PooledConnectionInner::Writer { guard })
            }
        };
        let elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.inner
            .metrics
            .read_acquires
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .metrics
            .read_wait_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
        Ok(out)
    }

    /// Acquire the serialized writer connection for mutations or transactions.
    pub fn write(&self) -> Result<PooledConnection<'_>> {
        let started = Instant::now();
        let guard = self.inner.writer.lock().expect("sqlite mutex poisoned");
        let elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.inner
            .metrics
            .write_acquires
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .metrics
            .write_wait_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
        Ok(PooledConnection(PooledConnectionInner::Writer { guard }))
    }

    /// Compatibility alias for acquiring the serialized writer connection.
    #[allow(dead_code)]
    pub fn lock(
        &self,
    ) -> std::result::Result<PooledConnection<'_>, PoisonError<MutexGuard<'_, Connection>>> {
        let started = Instant::now();
        let guard = self.inner.writer.lock()?;
        let elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.inner
            .metrics
            .write_acquires
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .metrics
            .write_wait_ns
            .fetch_add(elapsed_ns, Ordering::Relaxed);
        Ok(PooledConnection(PooledConnectionInner::Writer { guard }))
    }

    pub fn config(&self) -> &SqlitePoolConfig {
        &self.inner.config
    }

    pub fn checkpoint(&self, mode: WalCheckpointMode) -> Result<WalCheckpointResult> {
        let mode_str = mode.as_pragma_str();
        if self.inner.path.is_none() {
            return Ok(WalCheckpointResult {
                mode: mode_str.to_ascii_lowercase(),
                busy: 0,
                log_frames: 0,
                checkpointed_frames: 0,
            });
        }
        let db = self.write()?;
        let (busy, log_frames, checkpointed_frames) = db
            .query_row(&format!("PRAGMA wal_checkpoint({mode_str});"), [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .with_context(|| format!("run PRAGMA wal_checkpoint({mode_str})"))?;
        self.inner
            .metrics
            .checkpoints
            .fetch_add(1, Ordering::Relaxed);
        Ok(WalCheckpointResult {
            mode: mode_str.to_ascii_lowercase(),
            busy,
            log_frames,
            checkpointed_frames,
        })
    }

    pub fn pool_stats(&self) -> PoolStats {
        let (read_pool_max, read_pool_created, read_pool_idle, read_pool_in_use) =
            match &self.inner.readers {
                Some(pool) => pool.counts(),
                None => (0, 0, 0, 0),
            };
        PoolStats {
            is_in_memory: self.inner.path.is_none(),
            read_pool_max,
            read_pool_created,
            read_pool_idle,
            read_pool_in_use,
            read_acquires: self.inner.metrics.read_acquires.load(Ordering::Relaxed),
            write_acquires: self.inner.metrics.write_acquires.load(Ordering::Relaxed),
            read_waits: self.inner.metrics.read_waits.load(Ordering::Relaxed),
            read_wait_ms: self.inner.metrics.read_wait_ns.load(Ordering::Relaxed) / 1_000_000,
            write_wait_ms: self.inner.metrics.write_wait_ns.load(Ordering::Relaxed) / 1_000_000,
            checkpoints: self.inner.metrics.checkpoints.load(Ordering::Relaxed),
        }
    }

    pub fn diagnostics(&self) -> Result<SqliteDiagnostics> {
        let pragmas = {
            let db = self.write()?;
            query_live_pragmas(&db)?
        };
        let reader_query_only = if self.inner.readers.is_some() {
            let reader = self.read()?;
            let qo: i64 = reader
                .pragma_query_value(None, "query_only", |row| row.get(0))
                .context("query reader PRAGMA query_only")?;
            Some(qo != 0)
        } else {
            None
        };
        let files = self.inner.path.as_deref().map(stat_wal_files);
        Ok(SqliteDiagnostics {
            config: self.inner.config.clone(),
            pool: self.pool_stats(),
            pragmas: LivePragmas {
                reader_query_only,
                ..pragmas
            },
            files,
        })
    }
}

fn open_reader_connection(path: &Path, config: &SqlitePoolConfig) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open read-only sqlite connection to {}", path.display()))?;
    apply_common_pragmas(&conn, config)?;
    conn.pragma_update(None, "query_only", true)
        .context("set PRAGMA query_only on reader")?;
    Ok(conn)
}

fn apply_common_pragmas(conn: &Connection, config: &SqlitePoolConfig) -> Result<()> {
    conn.busy_timeout(config.busy_timeout())
        .context("set sqlite busy_timeout")?;
    conn.pragma_update(None, "temp_store", "MEMORY")
        .context("set PRAGMA temp_store = MEMORY")?;
    if config.cache_size_kib > 0 {
        conn.pragma_update(None, "cache_size", -config.cache_size_kib)
            .context("set PRAGMA cache_size")?;
    }
    Ok(())
}

fn apply_writer_pragmas(
    conn: &Connection,
    config: &SqlitePoolConfig,
    file_backed: bool,
) -> Result<()> {
    apply_common_pragmas(conn, config)?;
    if file_backed {
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .ok()
            .unwrap_or_default();
        if !mode.eq_ignore_ascii_case("wal") {
            let actual: String = conn
                .query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))
                .context("enable SQLite WAL journal_mode")?;
            if !actual.eq_ignore_ascii_case("wal") {
                return Err(anyhow!(
                    "expected SQLite journal_mode=wal, got '{actual}'"
                ));
            }
        }
        conn.pragma_update(None, "synchronous", config.synchronous.as_pragma_str())
            .context("set PRAGMA synchronous")?;
        conn.pragma_update(None, "wal_autocheckpoint", config.wal_autocheckpoint)
            .context("set PRAGMA wal_autocheckpoint")?;
        conn.pragma_update(None, "journal_size_limit", config.journal_size_limit_bytes)
            .context("set PRAGMA journal_size_limit")?;
    }
    Ok(())
}

fn query_live_pragmas(conn: &Connection) -> Result<LivePragmas> {
    let journal_mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .context("query PRAGMA journal_mode")?;
    let sync_code: i64 = conn
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .context("query PRAGMA synchronous")?;
    let synchronous = match sync_code {
        0 => "off".to_string(),
        1 => "normal".to_string(),
        2 => "full".to_string(),
        3 => "extra".to_string(),
        other => other.to_string(),
    };
    let busy_timeout_ms: i64 = conn
        .pragma_query_value(None, "busy_timeout", |row| row.get(0))
        .context("query PRAGMA busy_timeout")?;
    let wal_autocheckpoint: i64 = conn
        .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
        .context("query PRAGMA wal_autocheckpoint")?;
    let cache_size: i64 = conn
        .pragma_query_value(None, "cache_size", |row| row.get(0))
        .context("query PRAGMA cache_size")?;
    let journal_size_limit: i64 = conn
        .pragma_query_value(None, "journal_size_limit", |row| row.get(0))
        .context("query PRAGMA journal_size_limit")?;
    let temp_code: i64 = conn
        .pragma_query_value(None, "temp_store", |row| row.get(0))
        .context("query PRAGMA temp_store")?;
    let temp_store = match temp_code {
        0 => "default".to_string(),
        1 => "file".to_string(),
        2 => "memory".to_string(),
        other => other.to_string(),
    };
    Ok(LivePragmas {
        journal_mode,
        synchronous,
        busy_timeout_ms,
        wal_autocheckpoint,
        cache_size,
        journal_size_limit,
        temp_store,
        reader_query_only: None,
    })
}

fn stat_wal_files(db_path: &Path) -> WalFileStats {
    let file_len = |p: &Path| std::fs::metadata(p).ok().map(|m| m.len());
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
    WalFileStats {
        db_path: db_path.display().to_string(),
        db_bytes: file_len(db_path),
        wal_bytes: file_len(&wal_path),
        shm_bytes: file_len(&shm_path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDb {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("aircd-db-{label}-{}", uuid::Uuid::new_v4()));
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

    #[test]
    fn wal_pragmas_and_reader_query_only_are_applied_for_file_db() -> Result<()> {
        let tmp = TempDb::new("pragmas");
        let cfg = SqlitePoolConfig {
            read_pool_size: 4,
            busy_timeout_ms: 7500,
            synchronous: SqliteSynchronous::Normal,
            wal_autocheckpoint: 512,
            cache_size_kib: 8192,
            journal_size_limit_bytes: 33_554_432,
        };
        let pool = SharedConnection::open_pool(&tmp.path, cfg, |conn| {
            conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, val TEXT);")?;
            Ok(())
        })?;

        let diag = pool.diagnostics()?;
        assert_eq!(diag.pragmas.journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(diag.pragmas.synchronous, "normal");
        assert_eq!(diag.pragmas.busy_timeout_ms, 7500);
        assert_eq!(diag.pragmas.wal_autocheckpoint, 512);
        assert_eq!(diag.pragmas.cache_size, -8192);
        assert_eq!(diag.pragmas.journal_size_limit, 33_554_432);
        assert_eq!(diag.pragmas.temp_store, "memory");
        assert_eq!(diag.pragmas.reader_query_only, Some(true));
        assert!(!diag.pool.is_in_memory);
        assert_eq!(diag.pool.read_pool_max, 4);

        // Reader connections must reject accidental writes.
        {
            let reader = pool.read()?;
            assert!(reader
                .execute("INSERT INTO items (val) VALUES ('nope')", [])
                .is_err());
        }
        // Writer connection succeeds.
        {
            let writer = pool.write()?;
            writer.execute("INSERT INTO items (val) VALUES ('ok')", [])?;
        }
        let val: String = pool
            .read()?
            .query_row("SELECT val FROM items WHERE id = 1", [], |r| r.get(0))?;
        assert_eq!(val, "ok");
        Ok(())
    }

    #[test]
    fn concurrent_readers_scale_up_to_pool_size_without_blocking_each_other() -> Result<()> {
        let tmp = TempDb::new("concurrency");
        let cfg = SqlitePoolConfig {
            read_pool_size: 4,
            ..Default::default()
        };
        let pool = SharedConnection::open_pool(&tmp.path, cfg, |conn| {
            conn.execute_batch(
                "CREATE TABLE entries (id INTEGER PRIMARY KEY, body TEXT);
                 INSERT INTO entries (id, body) VALUES (1, 'hello');",
            )?;
            Ok(())
        })?;

        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || -> Result<String> {
                let reader = pool.read()?;
                // All 4 threads must hold distinct reader connections at the same time.
                barrier.wait();
                let body: String = reader.query_row(
                    "SELECT body FROM entries WHERE id = 1",
                    [],
                    |row| row.get(0),
                )?;
                Ok(body)
            }));
        }
        for handle in handles {
            assert_eq!(handle.join().unwrap()?, "hello");
        }
        let stats = pool.pool_stats();
        assert_eq!(stats.read_pool_created, 4);
        assert_eq!(stats.read_pool_idle, 4);
        assert_eq!(stats.read_pool_in_use, 0);
        assert_eq!(stats.read_acquires, 4);
        Ok(())
    }

    #[test]
    fn reader_pool_waits_and_reuses_connections_under_burst_beyond_pool_size() -> Result<()> {
        let tmp = TempDb::new("burst");
        let cfg = SqlitePoolConfig {
            read_pool_size: 2,
            ..Default::default()
        };
        let pool = SharedConnection::open_pool(&tmp.path, cfg, |conn| {
            conn.execute_batch("CREATE TABLE counters (n INTEGER NOT NULL); INSERT INTO counters VALUES (42);")?;
            Ok(())
        })?;

        let mut handles = Vec::new();
        for i in 0..16 {
            let pool = pool.clone();
            handles.push(std::thread::spawn(move || -> Result<i64> {
                if i % 5 == 0 {
                    let writer = pool.write()?;
                    writer.execute("UPDATE counters SET n = n + 0", [])?;
                }
                let reader = pool.read()?;
                std::thread::sleep(Duration::from_millis(10));
                let val: i64 = reader.query_row("SELECT n FROM counters", [], |r| r.get(0))?;
                Ok(val)
            }));
        }
        for handle in handles {
            assert_eq!(handle.join().unwrap()?, 42);
        }
        let stats = pool.pool_stats();
        assert_eq!(stats.read_pool_created, 2);
        assert!(stats.read_waits > 0, "expected burst to exercise pool wait");
        assert_eq!(stats.read_acquires, 16);
        Ok(())
    }

    #[test]
    fn external_connection_respects_busy_timeout_during_write_lock() -> Result<()> {
        let tmp = TempDb::new("busy");
        let cfg = SqlitePoolConfig {
            busy_timeout_ms: 3000,
            ..Default::default()
        };
        let pool = SharedConnection::open_pool(&tmp.path, cfg, |conn| {
            conn.execute_batch("CREATE TABLE t (x INTEGER);")?;
            Ok(())
        })?;

        // Hold an IMMEDIATE transaction on an external connection briefly,
        // then release it while pool.write() waits via busy_timeout.
        let external = Connection::open(&tmp.path)?;
        external.busy_timeout(Duration::from_millis(3000))?;
        external.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES (1);")?;

        let pool_clone = pool.clone();
        let writer_thread = std::thread::spawn(move || -> Result<()> {
            let conn = pool_clone.write()?;
            conn.execute("INSERT INTO t VALUES (2)", [])?;
            Ok(())
        });

        std::thread::sleep(Duration::from_millis(120));
        external.execute_batch("COMMIT;")?;
        writer_thread.join().unwrap()?;

        let count: i64 = pool
            .read()?
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))?;
        assert_eq!(count, 2);
        Ok(())
    }

    #[test]
    fn wal_checkpoint_and_pool_config_validation_work() -> Result<()> {
        let tmp = TempDb::new("checkpoint");
        let pool = SharedConnection::open_pool(&tmp.path, SqlitePoolConfig::default(), |conn| {
            conn.execute_batch("CREATE TABLE log (id INTEGER PRIMARY KEY, msg TEXT);")?;
            Ok(())
        })?;
        {
            let writer = pool.write()?;
            for i in 0..50 {
                writer.execute("INSERT INTO log (msg) VALUES (?1)", [format!("msg-{i}")])?;
            }
        }
        let res = pool.checkpoint(WalCheckpointMode::Truncate)?;
        assert_eq!(res.mode, "truncate");
        assert_eq!(res.busy, 0);
        assert_eq!(pool.pool_stats().checkpoints, 1);

        let bad = SqlitePoolConfig {
            read_pool_size: 0,
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        let bad2 = SqlitePoolConfig {
            cache_size_kib: -1,
            ..Default::default()
        };
        assert!(bad2.validate().is_err());
        let bad3 = SqlitePoolConfig {
            journal_size_limit_bytes: -2,
            ..Default::default()
        };
        assert!(bad3.validate().is_err());
        assert!(SqliteSynchronous::parse("bogus").is_err());
        assert!(WalCheckpointMode::parse("bogus").is_err());
        Ok(())
    }
}
