//! The SQLite-backed [`UsageStore`].
//!
//! Uses `rusqlite` with the `bundled` feature, so there is no system SQLite to
//! install and no runtime version negotiation. The amalgamation compiles into
//! the binary; the cost is build time, the benefit is that a user's launcher
//! works on a machine where nothing has ever been installed.
//!
//! # Migrations
//!
//! [`MIGRATIONS`] is an ordered list, applied once each, inside a single
//! transaction, on open. `user_version` carries how far the file has got, so
//! opening an old database is a forward migration and opening a current one is
//! a no-op. A migration that fails leaves the file untouched and returns
//! [`StoreError::Migration`]; there is no half-migrated state to debug.
//!
//! `user_version` rather than a side table, deliberately: it is a single
//! integer in the database header that SQLite already manages, so it cannot
//! drift out of step with the schema it describes.
//!
//! # Nothing here reads a clock or the environment
//!
//! [`SqliteUsageStore::open`] takes the path the caller resolved. That is the
//! only way this crate touches the filesystem, and it is why every test below
//! can use [`SqliteUsageStore::open_in_memory`] and run with no disk at all.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::frecency::{Frecency, Timestamp};
use crate::model::Source;

use super::{LaunchRecord, StoreError, UsageEntry, UsageStore};

/// `user_version` written once every migration in this list has been applied.
///
/// Bump this when a migration is appended. It is the single number that says
/// "this file is current", so it must move in lockstep with [`MIGRATIONS`].
const SCHEMA_VERSION: i64 = 1;

/// The ordered schema history.
///
/// Each entry is `(label, SQL)`. Append only: rewriting an applied migration
/// leaves existing files silently on the old schema, which is the failure mode
/// migrations exist to prevent.
const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_usage",
    r#"
    CREATE TABLE usage (
        item_id      TEXT    NOT NULL PRIMARY KEY,
        source       INTEGER NOT NULL,
        title        TEXT    NOT NULL,
        launches     INTEGER NOT NULL DEFAULT 0,
        last_launch  INTEGER,
        first_launch INTEGER NOT NULL
    ) STRICT;

    -- The recents query is "top N by last_launch", so the index is on exactly
    -- that. Ordering by id inside a last_launch tie is served by the primary
    -- key, so no second index is needed for the tiebreak.
    CREATE INDEX usage_by_last_launch ON usage (last_launch DESC, item_id ASC);
    "#,
)];

/// A [`UsageStore`] backed by SQLite.
#[derive(Debug)]
pub struct SqliteUsageStore {
    connection: Connection,
}

impl SqliteUsageStore {
    /// Opens (or creates) a database at `path`, migrating it forward first.
    ///
    /// The parent directory is created if missing, so a first run on a machine
    /// with no `%APPDATA%\Orca` works without the caller doing anything.
    pub fn open(path: impl AsRef<Path>) -> Result<SqliteUsageStore, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| StoreError::Open {
                path: Some(parent.display().to_string()),
                detail: error.to_string(),
            })?;
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| StoreError::Open {
            path: Some(path.display().to_string()),
            detail: error.to_string(),
        })?;
        SqliteUsageStore::from_connection(connection)
    }

    /// An empty in-memory database. No file, no disk, no cleanup.
    ///
    /// The connection is held for the store's lifetime, which is what keeps the
    /// database alive — an in-memory SQLite database is destroyed the moment
    /// its last connection closes.
    pub fn open_in_memory() -> Result<SqliteUsageStore, StoreError> {
        SqliteUsageStore::from_connection(Connection::open_in_memory().map_err(|error| {
            StoreError::Open {
                path: None,
                detail: error.to_string(),
            }
        })?)
    }

    /// Wraps an already-open connection, applying migrations.
    fn from_connection(connection: Connection) -> Result<SqliteUsageStore, StoreError> {
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| StoreError::Open {
                path: None,
                detail: error.to_string(),
            })?;
        SqliteUsageStore::migrate(&connection)?;

        // The migration loop reads `user_version` and advances it; this checks
        // that it landed where `SCHEMA_VERSION` says it should. A mismatch means
        // a migration was added without bumping the constant, which would
        // otherwise be invisible until a query failed against a missing column.
        let applied: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| StoreError::Migration {
                version: "verify".to_owned(),
                detail: error.to_string(),
            })?;
        if applied != SCHEMA_VERSION {
            return Err(StoreError::Migration {
                version: "verify".to_owned(),
                detail: format!(
                    "PRAGMA user_version is {applied} but this build declares {SCHEMA_VERSION}; \
                     a migration was added without bumping the constant"
                ),
            });
        }

        Ok(SqliteUsageStore { connection })
    }

    /// Applies every migration the file has not seen, in one transaction.
    fn migrate(connection: &Connection) -> Result<(), StoreError> {
        let transaction =
            connection
                .unchecked_transaction()
                .map_err(|error| StoreError::Migration {
                    version: "transaction".to_owned(),
                    detail: error.to_string(),
                })?;

        let current: i64 = transaction
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| StoreError::Migration {
                version: "user_version".to_owned(),
                detail: error.to_string(),
            })?;

        for (index, (label, sql)) in MIGRATIONS.iter().enumerate() {
            let version = index as i64 + 1;
            if version <= current {
                continue;
            }
            transaction
                .execute_batch(sql)
                .map_err(|error| StoreError::Migration {
                    version: (*label).to_owned(),
                    detail: error.to_string(),
                })?;
            // `user_version` only takes effect on assignment, and must be set
            // inside the same transaction as the DDL or a crash between the two
            // leaves a schema that claims to be older than it is.
            transaction
                .pragma_update(None, "user_version", version)
                .map_err(|error| StoreError::Migration {
                    version: (*label).to_owned(),
                    detail: error.to_string(),
                })?;
        }

        transaction.commit().map_err(|error| StoreError::Migration {
            version: "commit".to_owned(),
            detail: error.to_string(),
        })
    }

    /// The schema version the file is at.
    #[must_use]
    pub fn schema_version(&self) -> i64 {
        self.connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0)
    }

    /// The connection, for a caller that needs a query this store does not
    /// offer. Every statement here is a read.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

impl UsageStore for SqliteUsageStore {
    /// Merges a launch into the existing row, or inserts one.
    ///
    /// Written as a single `INSERT ... ON CONFLICT DO UPDATE` rather than a
    /// select-then-write. That is not a style preference: SQLite serialises
    /// writes, so a read-then-write pair is two statements and leaves a window
    /// in which a concurrent launcher instance loses a count. orca is
    /// single-instance, but the cost of doing it right here is zero and the
    /// bug it prevents is silent.
    fn record_launch(&mut self, record: LaunchRecord<'_>) -> Result<(), StoreError> {
        let source_index = record.source.index() as i64;
        let at = record.at.unix_seconds();
        self.connection
            .execute(
                "INSERT INTO usage (item_id, source, title, launches, last_launch, first_launch)
                 VALUES (?1, ?2, ?3, 1, ?4, ?4)
                 ON CONFLICT (item_id) DO UPDATE SET
                     launches    = launches + 1,
                     last_launch = MAX(usage.last_launch, excluded.last_launch),
                     source      = excluded.source,
                     title       = excluded.title",
                rusqlite::params![record.item_id, source_index, record.title, at],
            )
            .map_err(|error| StoreError::Query {
                context: "record_launch",
                detail: error.to_string(),
            })?;
        Ok(())
    }

    fn frecency(&self, item_id: &str) -> Result<Option<Frecency>, StoreError> {
        let raw: Option<(i64, Option<i64>)> = self
            .connection
            .query_row(
                "SELECT launches, last_launch FROM usage WHERE item_id = ?1",
                [item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| StoreError::Query {
                context: "frecency",
                detail: error.to_string(),
            })?;
        raw.map(|(launches, last)| decode_frecency(launches, last, item_id))
            .transpose()
    }

    fn entries(&self) -> Result<Vec<UsageEntry>, StoreError> {
        self.query_entries(
            "SELECT item_id, source, title, launches, last_launch FROM usage ORDER BY item_id",
            "entries",
            [],
        )
    }

    fn most_recent(&self, limit: usize) -> Result<Vec<UsageEntry>, StoreError> {
        self.query_entries(
            "SELECT item_id, source, title, launches, last_launch
             FROM usage
             WHERE last_launch IS NOT NULL
             ORDER BY last_launch DESC, item_id ASC
             LIMIT ?1",
            "most_recent",
            rusqlite::params![i64::try_from(limit).unwrap_or(i64::MAX)],
        )
    }

    fn forget(&mut self, item_id: &str) -> Result<bool, StoreError> {
        let removed = self
            .connection
            .execute("DELETE FROM usage WHERE item_id = ?1", [item_id])
            .map_err(|error| StoreError::Query {
                context: "forget",
                detail: error.to_string(),
            })?;
        Ok(removed > 0)
    }

    fn clear(&mut self) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM usage", [])
            .map_err(|error| StoreError::Query {
                context: "clear",
                detail: error.to_string(),
            })?;
        Ok(())
    }

    fn len(&self) -> Result<usize, StoreError> {
        self.connection
            .query_row("SELECT COUNT(*) FROM usage", [], |row| row.get::<_, i64>(0))
            .map(|count| count.max(0) as usize)
            .map_err(|error| StoreError::Query {
                context: "len",
                detail: error.to_string(),
            })
    }
}

impl SqliteUsageStore {
    /// Shared row decoding for [`UsageStore::entries`] and
    /// [`UsageStore::most_recent`].
    ///
    /// The query closure returns only plain columns, and every check that can
    /// fail with a `StoreError` happens afterwards. Doing it the other way round
    /// means boxing a `StoreError` into a `rusqlite::Error` and then unwrapping
    /// it again — which is how a real query error ends up reported as a
    /// conversion failure.
    fn query_entries<P>(
        &self,
        sql: &str,
        context: &'static str,
        params: P,
    ) -> Result<Vec<UsageEntry>, StoreError>
    where
        P: rusqlite::Params,
    {
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(|error| StoreError::Query {
                context,
                detail: error.to_string(),
            })?;
        let raw = statement
            .query_map(params, |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })
            .map_err(|error| StoreError::Query {
                context,
                detail: error.to_string(),
            })?;

        let mut entries = Vec::new();
        for row in raw {
            let (item_id, source_index, title, launches, last_launch) =
                row.map_err(|error| StoreError::Query {
                    context,
                    detail: error.to_string(),
                })?;
            let source = source_from_index(source_index, &item_id)?;
            let frecency = decode_frecency(launches, last_launch, &item_id)?;
            entries.push(UsageEntry {
                item_id,
                source,
                title,
                frecency,
            });
        }
        Ok(entries)
    }
}

/// Turns a persisted discriminator back into a [`Source`].
///
/// A value outside `0..Source::COUNT` means the row was written by a build that
/// knew a `Source` this one does not. Reporting it is the honest answer;
/// substituting `Source::Unknown` would silently attribute an application's
/// history to an unwired provider and quietly change the ranking.
fn source_from_index(index: i64, item_id: &str) -> Result<Source, StoreError> {
    let converted = usize::try_from(index).ok().filter(|i| *i < Source::COUNT);
    converted
        .and_then(Source::from_index)
        .ok_or_else(|| StoreError::Corrupt {
            item_id: item_id.to_owned(),
            detail: format!("source index {index} is not a known Source"),
        })
}

/// Builds a [`Frecency`] from raw columns, rejecting an impossible launch
/// count rather than silently clamping it.
///
/// A negative `launches` cannot be produced by any version of this code, so
/// seeing one means the file was written by something else. Reporting it is
/// better than ranking a row whose history is nonsense. A count above
/// `u32::MAX` saturates: that is 4 billion launches, and the only thing lost is
/// precision in a number that is already asymptotically flat.
fn decode_frecency(
    launches: i64,
    last_launch: Option<i64>,
    item_id: &str,
) -> Result<Frecency, StoreError> {
    if launches < 0 {
        return Err(StoreError::Corrupt {
            item_id: item_id.to_owned(),
            detail: format!("launch count {launches} is negative"),
        });
    }
    Ok(Frecency::new(
        u32::try_from(launches).unwrap_or(u32::MAX),
        last_launch.map(Timestamp::from_unix_seconds),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::usage_store_contract;
    use crate::store::UsageStoreExt;

    usage_store_contract!(
        sqlite_store_contract,
        SqliteUsageStore::open_in_memory().expect("in-memory db")
    );

    fn record(store: &mut SqliteUsageStore, id: &str, at: i64) {
        store
            .record_launch(LaunchRecord {
                item_id: id,
                source: Source::Application,
                title: id,
                at: Timestamp::from_unix_seconds(at),
            })
            .expect("launch");
    }

    #[test]
    fn a_fresh_database_is_at_the_current_schema_version() {
        let store = SqliteUsageStore::open_in_memory().expect("in-memory db");
        assert_eq!(store.schema_version(), SCHEMA_VERSION);
        assert_eq!(store.schema_version(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn re_running_the_migrations_on_a_current_database_is_a_no_op() {
        let mut store = SqliteUsageStore::open_in_memory().expect("in-memory db");
        record(&mut store, "app:note", 1_000);

        // Exactly what every `open` does. Applying it twice must neither fail
        // nor duplicate a row or lose history.
        SqliteUsageStore::migrate(store.connection()).expect("re-running is a no-op");
        SqliteUsageStore::migrate(store.connection()).expect("re-running is a no-op");

        assert_eq!(store.schema_version(), SCHEMA_VERSION);
        assert_eq!(
            store.len().expect("len"),
            1,
            "the table must not be recreated"
        );
        assert_eq!(
            store.frecency("app:note").expect("frecency"),
            Some(Frecency::new(1, Some(Timestamp::from_unix_seconds(1_000))))
        );
    }

    #[test]
    fn a_database_file_survives_reopening() {
        // A real file, in the system temp dir, because the in-memory store
        // cannot prove persistence. Removed at the end either way.
        let path = std::env::temp_dir().join(format!(
            "orca-core-sqlite-test-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        {
            let mut store = SqliteUsageStore::open(&path).expect("should open and create");
            assert!(path.exists(), "open must create the file");
            record(&mut store, "app:note", 1_000);
            record(&mut store, "app:note", 2_000);
            record(&mut store, "app:calc", 1_500);
        }

        {
            let mut store = SqliteUsageStore::open(&path).expect("should reopen");
            assert_eq!(
                store.frecency("app:note").expect("frecency"),
                Some(Frecency::new(2, Some(Timestamp::from_unix_seconds(2_000)))),
                "counts and timestamps must survive the file"
            );
            assert_eq!(store.len().expect("len"), 2);
            assert_eq!(store.schema_version(), SCHEMA_VERSION, "already migrated");
            // And it is writable again.
            record(&mut store, "app:note", 3_000);
            assert_eq!(
                store.frecency("app:note").expect("frecency"),
                Some(Frecency::new(3, Some(Timestamp::from_unix_seconds(3_000))))
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_parent_directory_is_created() {
        let base = std::env::temp_dir().join(format!("orca-core-sqlite-{}", std::process::id()));
        let path = base.join("nested").join("deeper").join("orca.db");
        let _ = std::fs::remove_dir_all(&base);

        let store = SqliteUsageStore::open(&path);
        assert!(store.is_ok(), "{:?}", store.err());
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_unopenable_path_reports_an_open_error_not_a_panic() {
        // A directory where the database file should be.
        let base =
            std::env::temp_dir().join(format!("orca-core-sqlite-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("fixture");

        let error = SqliteUsageStore::open(&base).expect_err("a directory is not a database");
        assert!(matches!(error, StoreError::Open { .. }), "{error}");
        assert!(
            error.to_string().contains("orca-core-sqlite-dir"),
            "{error}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_out_of_range_launch_timestamp_does_not_move_last_launch_backwards() {
        // `MAX(existing, excluded)` in the upsert. Without it, a launch recorded
        // from a machine whose clock is behind would silently erase a newer
        // timestamp and the row would rank as stale forever.
        let mut store = SqliteUsageStore::open_in_memory().expect("in-memory db");
        record(&mut store, "app:x", 5_000);
        record(&mut store, "app:x", 1_000);
        assert_eq!(
            store.frecency("app:x").expect("frecency"),
            Some(Frecency::new(2, Some(Timestamp::from_unix_seconds(5_000))))
        );
    }

    #[test]
    fn a_launch_at_the_epoch_is_not_treated_as_never() {
        // `last_launch` is NULL for "never", not 0. An epoch launch must be
        // distinguishable from an absent one, or the first launch of a
        // timestamp-fresh install vanishes from the recents list.
        let mut store = SqliteUsageStore::open_in_memory().expect("in-memory db");
        record(&mut store, "app:x", 0);
        assert_eq!(
            store.frecency("app:x").expect("frecency"),
            Some(Frecency::new(1, Some(Timestamp::EPOCH)))
        );
        assert_eq!(store.most_recent(10).expect("recent").len(), 1);
    }

    #[test]
    fn an_unknown_source_index_is_reported_as_corrupt() {
        // Simulates a row written by a newer build that added a Source variant.
        let mut store = SqliteUsageStore::open_in_memory().expect("in-memory db");
        record(&mut store, "app:x", 1);
        store
            .connection()
            .execute("UPDATE usage SET source = 999 WHERE item_id = 'app:x'", [])
            .expect("corrupt it");

        let error = store.entries().expect_err("must be reported, not guessed");
        assert!(matches!(error, StoreError::Corrupt { .. }), "{error}");
        assert!(error.to_string().contains("999"), "{error}");
    }

    #[test]
    fn a_negative_launch_count_is_reported_rather_than_clamped() {
        let mut store = SqliteUsageStore::open_in_memory().expect("in-memory db");
        record(&mut store, "app:x", 1);
        store
            .connection()
            .execute("UPDATE usage SET launches = -5 WHERE item_id = 'app:x'", [])
            .expect("corrupt it");

        let error = store.frecency("app:x").expect_err("must be reported");
        assert!(matches!(error, StoreError::Corrupt { .. }), "{error}");
        assert!(error.to_string().contains("negative"), "{error}");
    }

    #[test]
    fn the_recents_index_is_actually_used() {
        // Asserted against `EXPLAIN QUERY PLAN` rather than a timing: a
        // sequential scan here means the index is missing or the query changed
        // shape, and that is exactly the kind of regression a test suite
        // normally misses.
        let store = SqliteUsageStore::open_in_memory().expect("in-memory db");
        let plan = store
            .connection()
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT item_id, source, title, launches, last_launch
                 FROM usage
                 WHERE last_launch IS NOT NULL
                 ORDER BY last_launch DESC, item_id ASC
                 LIMIT ?1",
            )
            .expect("prepare")
            .query_row([1i64], |row| row.get::<_, String>(3))
            .expect("query plan");
        assert!(
            plan.contains("usage_by_last_launch"),
            "the recents query fell back to a scan: {plan}"
        );
    }

    #[test]
    fn migrations_are_ordered_and_labelled_uniquely() {
        // A duplicate label means a wrong `user_version` bump and a schema that
        // silently skips a step.
        let mut labels = std::collections::BTreeSet::new();
        for (label, sql) in MIGRATIONS {
            assert!(labels.insert(*label), "duplicate migration label {label}");
            assert!(!sql.trim().is_empty(), "migration {label} is empty");
        }
        assert_eq!(SCHEMA_VERSION, MIGRATIONS.len() as i64);
    }

    #[test]
    fn frecency_map_matches_the_memory_store_for_the_same_history() {
        let mut sqlite = SqliteUsageStore::open_in_memory().expect("in-memory db");
        let mut memory = crate::store::MemoryUsageStore::new();
        for (index, id) in ["app:a", "app:b", "app:c"].iter().enumerate() {
            let at = Timestamp::from_unix_seconds(100 + index as i64);
            let record = LaunchRecord {
                item_id: id,
                source: Source::Application,
                title: id,
                at,
            };
            sqlite.record_launch(record).expect("sqlite launch");
            memory.record_launch(record).expect("memory launch");
        }
        assert_eq!(
            sqlite.frecency_map().expect("sqlite map"),
            memory.frecency_map().expect("memory map")
        );
    }
}
