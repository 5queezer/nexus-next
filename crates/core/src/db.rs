use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions, Connection, Sqlite, SqlitePool, Transaction};

use crate::blob::BlobStore;
use crate::error::Result;

pub(crate) static MIGRATOR: Migrator = sqlx::migrate!();

pub const DB_FILE: &str = "nexus.db";

const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

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

/// SQLite creates its files by the umask, and `set-passphrase` runs outside the unit's `UMask`, so
/// the data directory's mode would be the only thing protecting them.
fn make_db_files_private(data_dir: &Path) -> io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let path = data_dir.join(format!("{DB_FILE}{suffix}"));
        match fs::set_permissions(path, Permissions::from_mode(0o600)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn side_file(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Opens and closes a database. If nothing else has it open, that replays a WAL left by a crash
/// into the database and removes it.
async fn replay_wal(target: &Path) {
    let connection = SqliteConnectOptions::new()
        .filename(target)
        .foreign_keys(false)
        .connect()
        .await;
    if let Ok(mut conn) = connection {
        let _ = sqlx::query("SELECT count(*) FROM sqlite_master")
            .fetch_one(&mut conn)
            .await;
        let _ = conn.close().await;
    }
}

/// Why the database at `target` must not be replaced or edited offline, if another process has it
/// open. Closing the last connection removes the WAL, so one still there after this function's own
/// open and close has another user. The server keeps a connection for as long as it runs, so the
/// signal also holds while it is idle.
pub async fn in_use_reason(target: &Path) -> io::Result<Option<String>> {
    replay_wal(target).await;
    let wal = side_file(target, "-wal");
    Ok(wal.try_exists()?.then(|| {
        format!(
            "{} is in use: {} is still there after closing it. Stop the server and try again. If \
             nothing has the database open, check that this user can write to the data directory: \
             a WAL left by a crash is replayed and removed by opening the database",
            target.display(),
            wal.display()
        )
    }))
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
            // An acknowledged submission must survive power loss. NORMAL syncs the WAL only before
            // a checkpoint, so a reboot could silently undo writes an agent already reported saved.
            .synchronous(SqliteSynchronous::Full)
            // Sustained load grows the WAL far past the autocheckpoint size. This caps what stays
            // on disk once a checkpoint lets it restart.
            .pragma("journal_size_limit", JOURNAL_SIZE_LIMIT.to_string())
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        // On one connection, before the pool can open a second. With the pool's connections alongside,
        // a cold start sometimes failed at once with "database is locked", whatever the busy timeout says.
        let mut first = options.connect().await?;
        MIGRATOR.run(&mut first).await?;
        first.close().await?;
        // Never closing idle connections keeps the WAL in place, which `in_use_reason` relies on.
        // `min_connections` alone is not enough: sqlx's reaper closes the last idle connection
        // before opening its replacement, and that close removes the WAL.
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        make_db_files_private(data_dir)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn database_files_are_private_whatever_the_umask() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        for name in ["nexus.db", "nexus.db-wal", "nexus.db-shm"] {
            let mode = fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        db.close().await;
    }

    #[tokio::test]
    async fn every_connection_syncs_fully_and_limits_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        // More connections than the first, because the pragmas are per connection.
        let mut held = Vec::new();
        for _ in 0..3 {
            let mut conn = db.conn().await.unwrap();
            let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            let limit: i64 = sqlx::query_scalar("PRAGMA journal_size_limit")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            assert_eq!((synchronous, limit), (2, JOURNAL_SIZE_LIMIT));
            held.push(conn);
        }
        drop(held);
        db.close().await;
    }

    #[tokio::test]
    async fn a_checkpoint_shrinks_a_wal_grown_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        sqlx::query("CREATE TABLE load (body BLOB)")
            .execute(db.pool())
            .await
            .unwrap();
        // An open read transaction stops checkpoints from restarting the WAL, as sustained load does.
        let mut reader = db.conn().await.unwrap();
        sqlx::query("BEGIN").execute(&mut *reader).await.unwrap();
        sqlx::query("SELECT count(*) FROM load")
            .fetch_one(&mut *reader)
            .await
            .unwrap();
        for _ in 0..90 {
            sqlx::query("INSERT INTO load VALUES (zeroblob(1048576))")
                .execute(db.pool())
                .await
                .unwrap();
        }
        let wal = dir.path().join("nexus.db-wal");
        let grown = fs::metadata(&wal).unwrap().len();
        assert!(grown > JOURNAL_SIZE_LIMIT as u64, "{grown}");
        sqlx::query("COMMIT").execute(&mut *reader).await.unwrap();
        drop(reader);

        sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO load VALUES (zeroblob(1))")
            .execute(db.pool())
            .await
            .unwrap();
        let after = fs::metadata(&wal).unwrap().len();
        assert!(after <= JOURNAL_SIZE_LIMIT as u64, "{after}");
        db.close().await;
    }

    #[tokio::test]
    async fn an_idle_pool_stays_visible_and_a_clean_close_still_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(DB_FILE);
        let db = Db::open(dir.path()).await.unwrap();
        let options = db.pool().options();
        assert_eq!(options.get_min_connections(), 1);
        assert_eq!(options.get_idle_timeout(), None);
        assert_eq!(options.get_max_lifetime(), None);
        sqlx::query("CREATE TABLE kept (x)")
            .execute(db.pool())
            .await
            .unwrap();
        assert!(in_use_reason(&target).await.unwrap().is_some());

        db.close().await;
        assert!(!dir.path().join("nexus.db-wal").exists());
        assert_eq!(in_use_reason(&target).await.unwrap(), None);
        let db = Db::open(dir.path()).await.unwrap();
        sqlx::query("SELECT * FROM kept")
            .fetch_all(db.pool())
            .await
            .unwrap();
        db.close().await;
    }

    #[tokio::test]
    async fn opening_tightens_a_database_left_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        Db::open(dir.path()).await.unwrap().close().await;
        let path = dir.path().join(DB_FILE);
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        let db = Db::open(dir.path()).await.unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        db.close().await;
    }
}
