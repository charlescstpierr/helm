//! SQLite access: one WAL-mode connection, embedded versioned migrations, and a small async
//! bridge that runs queries on Tokio's blocking pool.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;

/// Ordered, append-only list of schema migrations embedded in the binary.
/// Never edit a released migration: add a new one.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "init",
        sql: include_str!("../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "comments",
        sql: include_str!("../migrations/0002_comments.sql"),
    },
    Migration {
        version: 3,
        name: "autoincrement_comment_ids",
        sql: include_str!("../migrations/0003_autoincrement_comment_ids.sql"),
    },
];

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
    apply(conn, MIGRATIONS)
}

fn apply(conn: &mut Connection, migrations: &[Migration]) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             name       TEXT    NOT NULL,
             applied_at INTEGER NOT NULL DEFAULT (unixepoch())
         ) STRICT;",
    )?;
    let current = schema_version(conn)?;
    let supported = migrations.last().map_or(0, |m| m.version);
    if current > supported {
        return Err(DbError::SchemaTooNew {
            found: current,
            supported,
        });
    }
    for migration in migrations.iter().filter(|m| m.version > current) {
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
    fn a_v0_1_database_upgrades_without_losing_its_cards() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        apply(&mut conn, &MIGRATIONS[..1]).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 1);
        conn.execute_batch(
            "INSERT INTO cards (project_id, column_id, number, title, description, position)
                 VALUES (1, 2, 1, 'Existing', 'kept as is', 0);
             INSERT INTO labels (project_id, name, name_key, color_slot) VALUES (1, 'infra', 'infra', 3);
             INSERT INTO card_labels (card_id, label_id) VALUES (1, 1);",
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        assert_eq!(
            schema_version(&conn).unwrap(),
            MIGRATIONS.last().unwrap().version
        );
        let (title, description): (String, String) = conn
            .query_row(
                "SELECT title, description FROM cards WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (title.as_str(), description.as_str()),
            ("Existing", "kept as is")
        );
        assert_eq!(count(&conn, "card_labels"), 1);
        assert_eq!(count(&conn, "comments"), 0);
        conn.execute(
            "INSERT INTO comments (card_id, author_kind, author, body) VALUES (1, 'human', 'moi', 'hi')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn deleting_a_card_removes_its_comments_and_mentions() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO cards (project_id, column_id, number, title, position) VALUES (1, 1, 1, 'A', 0);
             INSERT INTO comments (card_id, author_kind, author, body) VALUES (1, 'human', 'moi', '@codex');
             INSERT INTO mentions (comment_id, target) VALUES (1, 'codex');",
        )
        .unwrap();

        conn.execute("DELETE FROM cards WHERE id = 1", []).unwrap();

        assert_eq!(count(&conn, "comments"), 0);
        assert_eq!(count(&conn, "mentions"), 0);
    }

    #[test]
    fn comments_reject_an_unknown_author_kind_and_an_orphan_card() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO cards (project_id, column_id, number, title, position) VALUES (1, 1, 1, 'A', 0)",
            [],
        )
        .unwrap();
        let insert = |card: i64, kind: &str| {
            conn.execute(
                "INSERT INTO comments (card_id, author_kind, author, body) VALUES (?1, ?2, 'x', 'y')",
                (card, kind),
            )
        };
        assert!(insert(1, "robot").is_err());
        assert!(insert(99, "human").is_err());
        assert!(insert(1, "system").is_ok());
    }

    #[test]
    fn comment_and_mention_ids_survive_the_autoincrement_rebuild_and_are_never_reused() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        apply(&mut conn, &MIGRATIONS[..2]).unwrap();
        conn.execute_batch(
            "INSERT INTO cards (project_id, column_id, number, title, position) VALUES
                 (1, 1, 1, 'A', 0), (1, 1, 2, 'B', 1);
             INSERT INTO comments (card_id, author_kind, author, body) VALUES
                 (1, 'human', 'moi', '@codex one'), (2, 'human', 'moi', '@claude two');
             INSERT INTO mentions (comment_id, target, handled_at) VALUES
                 (1, 'codex', 7), (2, 'claude', NULL);",
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        assert_eq!(count(&conn, "comments"), 2);
        let mentions: Vec<(i64, i64, String, Option<i64>)> = conn
            .prepare("SELECT id, comment_id, target, handled_at FROM mentions ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            mentions,
            [
                (1, 1, "codex".to_owned(), Some(7)),
                (2, 2, "claude".to_owned(), None)
            ]
        );

        // Deleting the newest card removes its newest comment and mention; the next ones must
        // not take their ids.
        conn.execute("DELETE FROM cards WHERE id = 2", []).unwrap();
        assert_eq!(count(&conn, "comments"), 1);
        assert_eq!(count(&conn, "mentions"), 1);
        conn.execute(
            "INSERT INTO comments (card_id, author_kind, author, body) VALUES (1, 'human', 'moi', '@moi three')",
            [],
        )
        .unwrap();
        let comment_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO mentions (comment_id, target) VALUES (?1, 'moi')",
            [comment_id],
        )
        .unwrap();
        let mention_id = conn.last_insert_rowid();
        assert_eq!((comment_id, mention_id), (3, 3));

        // The rebuilt foreign keys still cascade, and still point at the live tables.
        conn.execute("DELETE FROM cards WHERE id = 1", []).unwrap();
        assert_eq!(count(&conn, "comments"), 0);
        assert_eq!(count(&conn, "mentions"), 0);
        let dangling: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(dangling, 0);
    }

    #[test]
    fn unhandled_mentions_are_served_by_a_partial_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrate(&mut conn).unwrap();
        let plan: String = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT comment_id FROM mentions
                 WHERE target = 'codex' AND handled_at IS NULL ORDER BY comment_id",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("; ");
        assert!(plan.contains("mentions_unhandled"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
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
