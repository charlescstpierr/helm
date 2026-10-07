//! SQLite access: one WAL-mode connection, embedded versioned migrations, and a small async
//! bridge that runs queries on Tokio's blocking pool.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;

/// Ordered, append-only list of schema migrations embedded in the binary.
/// Never edit a released migration: add a new one.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "init",
    sql: include_str!("../migrations/0001_init.sql"),
}];

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

#[derive(Debug)]
pub enum DbError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    /// The database was written by a newer Helm than this binary.
    SchemaTooNew {
        found: i64,
        supported: i64,
    },
    /// The blocking task running a query panicked or was cancelled.
    Task(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::Io(e) => write!(f, "database file: {e}"),
            Self::SchemaTooNew { found, supported } => write!(
                f,
                "database schema is at version {found}, this binary supports up to {supported}"
            ),
            Self::Task(e) => write!(f, "database task: {e}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

/// Cheaply cloneable handle on the single SQLite connection.
///
/// A solo-developer board has no write contention worth a pool; one connection keeps memory
/// low and makes every operation trivially serialized. WAL still matters: it lets external
/// readers (the `sqlite3` CLI, a backup, later agent tooling) read while Helm writes.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Opens (creating if needed) the database file and brings its schema up to date.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(DbError::Io)?;
        }
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, DbError> {
        Self::init(Connection::open_in_memory()?)
    }

    /// A bare, fully migrated in-memory connection for synchronous store tests.
    #[cfg(test)]
    pub fn test_connection() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    fn init(mut conn: Connection) -> Result<Self, DbError> {
        configure(&conn)?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs `f` with exclusive access to the connection, off the async runtime thread.
    pub async fn call<F, T, E>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            // A poisoned lock only means an earlier query panicked; SQLite rolled its
            // transaction back, so the connection itself is still consistent.
            let mut guard = conn.lock().unwrap_or_else(|poison| poison.into_inner());
            f(&mut guard)
        })
        .await
        .map_err(|e| E::from(DbError::Task(e.to_string())))?
    }

    /// Folds the WAL back into the main file; called on clean shutdown.
    pub fn checkpoint(&self) {
        let guard = self
            .conn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _ = guard.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    }
}

fn configure(conn: &Connection) -> Result<(), DbError> {
    conn.busy_timeout(Duration::from_secs(5))?;
    // `journal_mode` returns the resulting mode as a row (in-memory databases stay "memory").
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             name       TEXT    NOT NULL,
             applied_at INTEGER NOT NULL DEFAULT (unixepoch())
         ) STRICT;",
    )?;
    let current = schema_version(conn)?;
    let supported = MIGRATIONS.last().map_or(0, |m| m.version);
    if current > supported {
        return Err(DbError::SchemaTooNew {
            found: current,
            supported,
        });
    }
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
            (migration.version, migration.name),
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn schema_version(conn: &Connection) -> Result<i64, DbError> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn migration_versions_are_contiguous_from_one() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(migration.version, index as i64 + 1, "{}", migration.name);
        }
    }

    #[test]
    fn migrations_seed_default_board_and_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();

        assert_eq!(
            schema_version(&conn).unwrap(),
            MIGRATIONS.last().unwrap().version
        );
        assert_eq!(count(&conn, "schema_migrations"), MIGRATIONS.len() as i64);
        assert_eq!(count(&conn, "projects"), 1);

        let names: Vec<String> = conn
            .prepare("SELECT name FROM board_columns ORDER BY position")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            names,
            ["Backlog", "À faire", "En cours", "En revue", "Terminé"]
        );
    }

    #[test]
    fn refuses_a_database_from_a_newer_binary() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name) VALUES (9999, 'future')",
            [],
        )
        .unwrap();
        assert!(matches!(
            migrate(&mut conn),
            Err(DbError::SchemaTooNew { found: 9999, .. })
        ));
    }

    #[test]
    fn file_database_uses_wal_and_enforces_foreign_keys() {
        let dir = std::env::temp_dir().join(format!("helm-db-test-{}", std::process::id()));
        let path = dir.join("nested").join("helm.db");
        let db = Db::open(&path).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            let foreign_keys: i64 = conn
                .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                .unwrap();
            assert_eq!(foreign_keys, 1);
        }
        db.checkpoint();
        drop(db);
        // Reopening an up-to-date database is a no-op.
        Db::open(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
