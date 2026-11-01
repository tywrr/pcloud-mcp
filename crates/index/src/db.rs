//! The SQLite file: opening (and rebuilding a corrupt one), the schema, and connections.
//!
//! One writer connection behind a mutex (walks run one at a time anyway) and a few reader
//! connections, which WAL lets run alongside the writer.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

/// Bump with a migration step in [`migrate`].
const SCHEMA_VERSION: i64 = 2;
/// Readers kept open between queries.
const MAX_IDLE_READERS: usize = 4;

const SCHEMA: &str = "
CREATE TABLE entries(
    id INTEGER PRIMARY KEY,
    remote_id TEXT,
    path TEXT NOT NULL UNIQUE,
    parent TEXT NOT NULL,
    name TEXT NOT NULL,
    name_fold TEXT NOT NULL,
    is_dir INTEGER NOT NULL,
    -- files: bytes (0 when unknown); folders: NULL
    size INTEGER,
    -- Unix seconds, 0 when unknown, so it can be indexed and compared without NULLs
    mtime INTEGER NOT NULL DEFAULT 0,
    ext TEXT,
    kind TEXT NOT NULL,
    root TEXT NOT NULL,
    gen INTEGER NOT NULL,
    -- set by our own writes; a walk that started before them (or in the same second) leaves
    -- the row alone
    own_write_at INTEGER,
    -- a tombstone: deleted by us, hidden from queries until a later walk drops it
    deleted_at INTEGER
);
CREATE INDEX entries_parent ON entries(parent);
CREATE INDEX entries_root ON entries(root);
CREATE INDEX entries_size ON entries(size, path) WHERE is_dir = 0 AND deleted_at IS NULL;
CREATE INDEX entries_mtime ON entries(mtime, path);
CREATE INDEX entries_remote_id ON entries(remote_id) WHERE remote_id IS NOT NULL;

-- keyed by remote_id, or 'path:' || path for backends without ids; valid while size and mtime
-- still match the entry
CREATE TABLE hashes(remote_id TEXT PRIMARY KEY, size INTEGER, mtime INTEGER,
                    sha1 TEXT, md5 TEXT, sha256 TEXT);

CREATE TABLE folder_totals(path TEXT PRIMARY KEY, files INTEGER NOT NULL, dirs INTEGER NOT NULL,
                           bytes INTEGER NOT NULL, newest INTEGER) WITHOUT ROWID;

CREATE TABLE changes(seq INTEGER PRIMARY KEY, at INTEGER NOT NULL, op TEXT NOT NULL,
                     path TEXT NOT NULL, old_path TEXT, size INTEGER,
                     is_dir INTEGER NOT NULL DEFAULT 0, source TEXT NOT NULL);
CREATE INDEX changes_at ON changes(at);

CREATE TABLE roots(path TEXT PRIMARY KEY, every_s INTEGER NOT NULL, walk_started INTEGER,
                   last_ok INTEGER, last_ms INTEGER, last_error TEXT, gen INTEGER,
                   entries INTEGER NOT NULL DEFAULT 0,
                   pending_removals INTEGER, pending_at INTEGER, pending_of INTEGER);
-- the removals a walk held back, applied by refresh_index confirm=true; gen is the generation
-- of the walk that held it (an entry with a newer gen has been seen or written since)
CREATE TABLE held(root TEXT NOT NULL, entry_id INTEGER NOT NULL, gen INTEGER NOT NULL DEFAULT 0,
                  PRIMARY KEY(root, entry_id)) WITHOUT ROWID;

CREATE TABLE meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
INSERT INTO meta(key, value) VALUES ('generation', 0);
";

pub struct Db {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("path", &self.path).finish()
    }
}

/// A reader borrowed from the pool, returned when dropped.
pub struct Reader<'a> {
    conn: Option<Connection>,
    pool: &'a Mutex<Vec<Connection>>,
}

impl std::ops::Deref for Reader<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("a reader holds its connection until dropped")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut pool = lock(self.pool);
            if pool.len() < MAX_IDLE_READERS {
                pool.push(conn);
            }
        }
    }
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // SQLite connections stay usable after a panic elsewhere; transactions roll back on drop.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Db {
    /// Opens (or creates) `index.db` in `dir`. A file that fails SQLite's quick check, or
    /// can't be opened at all, is moved aside and a fresh one is started: the index is a
    /// cache, rebuilt by the next walks.
    pub fn open(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("can't create the index directory {}: {e}", dir.display()))?;
        let path = dir.join("index.db");
        match Self::open_checked(&path) {
            Ok(db) => Ok(db),
            Err(why) => {
                let aside = dir.join(format!(
                    "index.db.corrupt-{}",
                    chance_of_files_core::time::now()
                ));
                tracing::error!(error = %why, moved_to = %aside.display(), "index.db is unusable; starting a new one");
                std::fs::rename(&path, &aside)
                    .map_err(|e| format!("can't move the broken index aside: {e}"))?;
                for suffix in ["-wal", "-shm"] {
                    let side = dir.join(format!("index.db{suffix}"));
                    let _ = std::fs::rename(&side, aside.with_extension(format!("db{suffix}")));
                }
                Self::open_checked(&path)
            }
        }
    }

    fn open_checked(path: &Path) -> Result<Self, String> {
        let writer = connect(path).map_err(|e| e.to_string())?;
        let check: String = writer
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if check != "ok" {
            return Err(format!("quick_check: {check}"));
        }
        migrate(&writer).map_err(|e| e.to_string())?;
        Ok(Self {
            path: path.to_owned(),
            writer: Mutex::new(writer),
            readers: Mutex::new(Vec::new()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes on disk, the WAL included.
    pub fn bytes(&self) -> u64 {
        ["", "-wal"]
            .iter()
            .filter_map(|suffix| {
                let mut name = self.path.as_os_str().to_owned();
                name.push(suffix);
                std::fs::metadata(PathBuf::from(name)).ok()
            })
            .map(|m| m.len())
            .sum()
    }

    pub fn writer(&self) -> MutexGuard<'_, Connection> {
        lock(&self.writer)
    }

    pub fn reader(&self) -> rusqlite::Result<Reader<'_>> {
        let idle = lock(&self.readers).pop();
        let conn = match idle {
            Some(conn) => conn,
            None => connect(&self.path)?,
        };
        Ok(Reader {
            conn: Some(conn),
            pool: &self.readers,
        })
    }
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    // NORMAL is safe with WAL: a crash can lose the last commits, never corrupt the file, and
    // the index is a cache anyway. Temp tables (a walk's listing, ~130k rows for Immich) stay
    // in memory: the pod's /tmp is a small emptyDir. The WAL is cut back to 32 MB after each
    // checkpoint, or it keeps the size of the largest walk (~90 MB).
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA journal_size_limit = 33554432;
         PRAGMA cache_size = -16000;",
    )?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    let has_tables: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'entries'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if has_tables.is_some() || version > SCHEMA_VERSION {
        // An unknown layout: it is only a cache, so start over rather than guess.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for table in tables {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS \"{table}\""))?;
        }
    }
    conn.execute_batch(&format!(
        "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
    ))
}
