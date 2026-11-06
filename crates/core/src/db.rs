use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::blob::BlobStore;
use crate::error::Result;

pub(crate) static MIGRATOR: Migrator = sqlx::migrate!();

pub const DB_FILE: &str = "nexus.db";

/// Creates the data directory, private to the service user, if it does not exist yet.
pub(crate) fn create_data_dir(data_dir: &Path) -> std::io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(data_dir)
}

pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILE)
}

/// The database pool (WAL, foreign keys on) plus the blob store of one data directory.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
    blobs: BlobStore,
}

impl Db {
    /// Opens `<data_dir>/nexus.db`, creating it if needed, and applies pending migrations.
    pub async fn open(data_dir: &Path) -> Result<Self> {
        create_data_dir(data_dir)?;
        let options = SqliteConnectOptions::new()
            .filename(db_path(data_dir))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            blobs: BlobStore::new(data_dir),
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// A connection for reads and single-statement writes.
    pub async fn conn(&self) -> Result<PoolConnection<Sqlite>> {
        Ok(self.pool.acquire().await?)
    }

    /// Starts a transaction holding SQLite's write lock from the first statement. Use it for any
    /// read-then-write unit: a deferred transaction that upgrades after another writer committed
    /// fails with SQLITE_BUSY at once, ignoring the busy timeout. It also stands in for old
    /// Nexus's `SELECT ... FOR UPDATE` serialization.
    pub async fn begin_write(&self) -> Result<Transaction<'static, Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    /// Closes the pool, checkpointing the WAL.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
