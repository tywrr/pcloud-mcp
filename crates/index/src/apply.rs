//! Writing to the index: applying a walk (with its guards), held removals, our own writes,
//! folder totals and the startup clean-up.
//!
//! A walk's listing goes into a temporary table and is diffed against `entries` in SQL, so
//! applying 130k entries never needs both sides in memory.

use std::collections::{HashMap, HashSet};

use chance_of_files_core::{Entry, Policy, fold, kind, time};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::db::Db;
use crate::paths::{parent_of, under_range};

/// A walk that removes fewer entries than this is never held, whatever the share: a small
/// root losing a handful of files is not what the guard is for.
const MIN_HELD_REMOVALS: u64 = 10;
/// The change log keeps this many days...
const CHANGES_KEEP_S: i64 = 90 * 86_400;
/// ...and at most this many rows.
const CHANGES_KEEP_ROWS: i64 = 200_000;

/// One listed entry, as the walk keeps it until it is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkRow {
    pub path: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub remote_id: Option<String>,
}

impl From<Entry> for WalkRow {
    fn from(e: Entry) -> Self {
        Self {
            mtime: e.modified.as_deref().and_then(time::parse_rfc3339),
            path: e.path,
            is_dir: e.is_dir,
            size: e.size,
            remote_id: e.id,
        }
    }
}

/// What applying a walk did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub added: u64,
    pub removed: u64,
    pub changed: u64,
    pub moved: u64,
    /// Removals held back by the guard.
    pub held: u64,
    /// The root's entries afterwards.
    pub entries: u64,
}

/// The parts of a walk that say what it covered.
#[derive(Debug, Clone)]
pub struct WalkScope<'a> {
    pub root: &'a str,
    /// The folder walked: `root` for a full walk, or a folder below it.
    pub at: &'a str,
    /// When the listing started (Unix seconds); own writes and deletes at or after it win
    /// over what it saw (a tie goes to our own write: the listing may predate it).
    pub started: i64,
    pub max_removal: f64,
    /// Apply removals even past the guard (a confirmed refresh).
    pub confirmed: bool,
}

impl WalkScope<'_> {
    fn full(&self) -> bool {
        self.at == self.root
    }
}

fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

fn sql_size(size: Option<u64>) -> Option<i64> {
    size.map(|s| i64::try_from(s).unwrap_or(i64::MAX))
}

/// Column values derived from a row.
struct Derived<'a> {
    parent: &'a str,
    name: &'a str,
    name_fold: String,
    ext: Option<String>,
    kind: &'static str,
    size: Option<i64>,
    mtime: i64,
}

fn derive(row: &WalkRow) -> Derived<'_> {
    let name = name_of(&row.path);
    let ext = (!row.is_dir).then(|| kind::extension(name)).flatten();
    Derived {
        parent: parent_of(&row.path),
        name,
        name_fold: fold(name),
        kind: kind::kind_of(row.is_dir, ext.as_deref()),
        ext,
        size: if row.is_dir {
            None
        } else {
            Some(sql_size(row.size).unwrap_or(0))
        },
        mtime: row.mtime.unwrap_or(0),
    }
}

fn next_generation(tx: &Transaction<'_>) -> rusqlite::Result<i64> {
    tx.execute(
        "UPDATE meta SET value = value + 1 WHERE key = 'generation'",
        [],
    )?;
    tx.query_row("SELECT value FROM meta WHERE key = 'generation'", [], |r| {
        r.get(0)
    })
}

/// Logs changes, leaving out those whose parent has the same change (a removed folder is one
/// line, not one per file inside it).
struct ChangeRow {
    op: &'static str,
    path: String,
    old: Option<String>,
    size: Option<i64>,
    is_dir: bool,
}

struct ChangeLog {
    at: i64,
    source: &'static str,
    rows: Vec<ChangeRow>,
}

impl ChangeLog {
    fn new(source: &'static str) -> Self {
        Self {
            at: time::now(),
            source,
            rows: Vec::new(),
        }
    }

    fn push(
        &mut self,
        op: &'static str,
        path: &str,
        old: Option<&str>,
        size: Option<i64>,
        is_dir: bool,
    ) {
        self.rows.push(ChangeRow {
            op,
            path: path.to_owned(),
            old: old.map(str::to_owned),
            size,
            is_dir,
        });
    }

    /// Keeps only the topmost of each op: a row whose parent has the same op (and, for
    /// moves, moved to the new path's parent) is covered by its parent's row.
    fn collapse(&mut self) {
        let by_op: HashMap<&str, HashMap<&str, Option<&str>>> = {
            let mut map: HashMap<&str, HashMap<&str, Option<&str>>> = HashMap::new();
            for row in &self.rows {
                let key = if row.op == "moved" {
                    row.old.as_deref().unwrap_or(&row.path)
                } else {
                    &row.path
                };
                map.entry(row.op)
                    .or_default()
                    .insert(key, Some(row.path.as_str()));
            }
            map
        };
        let keep: Vec<bool> = self
            .rows
            .iter()
            .map(|row| {
                let ops = &by_op[row.op];
                if row.op == "moved" {
                    let old = row.old.as_deref().unwrap_or(&row.path);
                    match ops.get(parent_of(old)) {
                        Some(Some(new_parent)) => *new_parent != parent_of(&row.path),
                        _ => true,
                    }
                } else {
                    !ops.contains_key(parent_of(&row.path))
                }
            })
            .collect();
        let mut keep = keep.into_iter();
        self.rows.retain(|_| keep.next().unwrap_or(true));
    }

    fn write(mut self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        self.collapse();
        let mut insert = tx.prepare_cached(
            "INSERT INTO changes(at, op, path, old_path, size, is_dir, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for r in &self.rows {
            insert.execute(params![
                self.at,
                r.op,
                r.path,
                r.old,
                r.size,
                r.is_dir,
                self.source
            ])?;
        }
        Ok(())
    }
}

impl Db {
    /// Records that a walk of `root` is starting (a full walk only).
    pub fn walk_starting(&self, root: &str, started: i64) -> rusqlite::Result<()> {
        self.writer().execute(
            "UPDATE roots SET walk_started = ?2 WHERE path = ?1",
            params![root, started],
        )?;
        Ok(())
    }

    pub fn walk_failed(&self, root: &str, error: &str) -> rusqlite::Result<()> {
        self.writer().execute(
            "UPDATE roots SET last_error = ?2 WHERE path = ?1",
            params![root, error],
        )?;
        Ok(())
    }

    /// Applies a walk's listing of `scope.at`. See the module docs of `crate::indexer` for
    /// the rules; in short: adds, changes and moves always apply, removals apply unless they
    /// pass the guard, and rows our own writes touched after the walk started are left alone.
    pub fn apply_walk(
        &self,
        scope: &WalkScope<'_>,
        rows: Vec<WalkRow>,
        took_ms: u64,
    ) -> rusqlite::Result<Applied> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        let generation_no = next_generation(&tx)?;
        let applied = apply_in(&tx, scope, rows, generation_no)?;
        if scope.full() {
            tx.execute(
                "UPDATE roots SET last_ok = walk_started, last_ms = ?2, last_error = NULL, gen = ?3,
                        entries = ?4
                 WHERE path = ?1",
                params![scope.root, took_ms as i64, generation_no, applied.entries as i64],
            )?;
        } else {
            tx.execute(
                "UPDATE roots SET entries = ?2 WHERE path = ?1",
                params![scope.root, applied.entries as i64],
            )?;
        }
        recompute_totals(&tx)?;
        prune_changes(&tx)?;
        tx.commit()?;
        Ok(applied)
    }

    /// Applies the removals a walk of `root` held back, except for entries a later walk saw
    /// or our own write touched (their generation is newer than the hold). Returns how many
    /// were removed.
    pub fn apply_held(&self, root: &str) -> rusqlite::Result<u64> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        let gone: Vec<(i64, String, Option<i64>, bool)> = tx
            .prepare(
                "SELECT e.id, e.path, coalesce(t.bytes, e.size), e.is_dir FROM held h
                 JOIN entries e ON e.id = h.entry_id
                 LEFT JOIN folder_totals t ON t.path = e.path
                 WHERE h.root = ?1 AND e.deleted_at IS NULL AND e.gen <= h.gen",
            )?
            .query_map([root], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        let mut log = ChangeLog::new("walk");
        {
            let mut delete = tx.prepare_cached("DELETE FROM entries WHERE id = ?1")?;
            for (id, path, size, is_dir) in &gone {
                delete.execute([id])?;
                log.push("removed", path, None, *size, *is_dir);
            }
        }
        log.write(&tx)?;
        tx.execute("DELETE FROM held WHERE root = ?1", [root])?;
        let entries: i64 = tx.query_row(
            "SELECT count(*) FROM entries WHERE root = ?1 AND deleted_at IS NULL",
            [root],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE roots SET pending_removals = NULL, pending_at = NULL, pending_of = NULL,
                    entries = ?2
             WHERE path = ?1",
            params![root, entries],
        )?;
        next_generation(&tx)?;
        recompute_totals(&tx)?;
        tx.commit()?;
        Ok(gone.len() as u64)
    }

    /// Records a file or folder we just wrote (and pCloud confirmed). A walk that started
    /// before `at`, or in the same second, leaves the row as written here.
    pub fn record_own_write(&self, root: &str, row: &WalkRow, at: i64) -> rusqlite::Result<()> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        let generation_no = next_generation(&tx)?;
        let d = derive(row);
        let existed: Option<i64> = tx
            .query_row(
                "SELECT id FROM entries WHERE path = ?1 AND deleted_at IS NULL",
                [&row.path],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO entries(remote_id, path, parent, name, name_fold, is_dir, size, mtime,
                                 ext, kind, root, gen, own_write_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, NULL)
             ON CONFLICT(path) DO UPDATE SET remote_id = excluded.remote_id,
                 is_dir = excluded.is_dir, size = excluded.size, mtime = excluded.mtime,
                 ext = excluded.ext, kind = excluded.kind, root = excluded.root,
                 gen = excluded.gen, own_write_at = excluded.own_write_at, deleted_at = NULL",
            params![
                row.remote_id,
                row.path,
                d.parent,
                d.name,
                d.name_fold,
                row.is_dir,
                d.size,
                d.mtime,
                d.ext,
                d.kind,
                root,
                generation_no,
                at
            ],
        )?;
        let mut log = ChangeLog::new("own-write");
        log.at = at;
        log.push(
            if existed.is_some() {
                "changed"
            } else {
                "added"
            },
            &row.path,
            None,
            d.size,
            row.is_dir,
        );
        log.write(&tx)?;
        recompute_totals(&tx)?;
        tx.commit()
    }

    /// Records that we deleted `path` (and everything under it): tombstones, so a walk that
    /// listed before the delete doesn't bring it back.
    pub fn record_own_delete(&self, path: &str, at: i64) -> rusqlite::Result<u64> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        next_generation(&tx)?;
        let size: Option<i64> = tx
            .query_row(
                "SELECT coalesce(t.bytes, e.size) FROM entries e
                 LEFT JOIN folder_totals t ON t.path = e.path WHERE e.path = ?1",
                [path],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let (lo, hi) = under_range(path);
        let n = tx.execute(
            "UPDATE entries SET deleted_at = ?2
             WHERE deleted_at IS NULL AND (path = ?1 OR (path >= ?3 AND path < ?4))",
            params![path, at, lo, hi],
        )?;
        let mut log = ChangeLog::new("own-write");
        log.at = at;
        log.push("removed", path, None, size, false);
        log.write(&tx)?;
        recompute_totals(&tx)?;
        tx.commit()?;
        Ok(n as u64)
    }

    /// Brings stored rows in line with the configuration: forgets roots that are gone, moves
    /// rows to the root that now holds them, and drops anything now hidden or outside every
    /// root. Returns how many rows changed.
    pub fn reconcile(&self, roots: &[(String, u64)], policy: &Policy) -> rusqlite::Result<u64> {
        let mut conn = self.writer();
        let tx = conn.transaction()?;
        let configured: HashSet<&str> = roots.iter().map(|(p, _)| p.as_str()).collect();
        let stored: Vec<String> = tx
            .prepare("SELECT path FROM roots")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for old in stored.iter().filter(|p| !configured.contains(p.as_str())) {
            tx.execute("DELETE FROM roots WHERE path = ?1", [old])?;
            tx.execute("DELETE FROM held WHERE root = ?1", [old])?;
        }
        for (path, every) in roots {
            tx.execute(
                "INSERT INTO roots(path, every_s) VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET every_s = excluded.every_s",
                params![path, *every as i64],
            )?;
        }

        let folded: Vec<(String, &str)> =
            roots.iter().map(|(p, _)| (fold(p), p.as_str())).collect();
        let owner = |path: &str| -> Option<&str> {
            let path = fold(path);
            folded
                .iter()
                .filter(|(root, _)| {
                    root == "/"
                        || (path.len() > root.len()
                            && path.starts_with(root.as_str())
                            && path.as_bytes()[root.len()] == b'/')
                })
                .max_by_key(|(root, _)| root.len())
                .map(|(_, original)| *original)
        };
        let mut fixes: Vec<(i64, Option<String>)> = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, path, root FROM entries")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let (id, path, root): (i64, String, String) =
                    (row.get(0)?, row.get(1)?, row.get(2)?);
                if !policy.visible(&path) {
                    fixes.push((id, None));
                    continue;
                }
                match owner(&path) {
                    None => fixes.push((id, None)),
                    Some(want) if want != root => fixes.push((id, Some(want.to_owned()))),
                    Some(_) => {}
                }
            }
        }
        for (id, root) in &fixes {
            match root {
                None => tx.execute("DELETE FROM entries WHERE id = ?1", [id])?,
                Some(root) => tx.execute(
                    "UPDATE entries SET root = ?2 WHERE id = ?1",
                    params![id, root],
                )?,
            };
        }
        if !fixes.is_empty() {
            for (path, _) in roots {
                tx.execute(
                    "UPDATE roots SET entries =
                        (SELECT count(*) FROM entries WHERE root = ?1 AND deleted_at IS NULL)
                     WHERE path = ?1",
                    [path],
                )?;
            }
            recompute_totals(&tx)?;
        }
        tx.commit()?;
        Ok(fixes.len() as u64)
    }
}

fn apply_in(
    tx: &Transaction<'_>,
    scope: &WalkScope<'_>,
    rows: Vec<WalkRow>,
    generation_no: i64,
) -> rusqlite::Result<Applied> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS w(path TEXT PRIMARY KEY, remote_id TEXT, parent TEXT,
             name TEXT, name_fold TEXT, is_dir INTEGER, size INTEGER, mtime INTEGER, ext TEXT,
             kind TEXT);
         CREATE INDEX IF NOT EXISTS temp.w_remote_id ON w(remote_id);
         DELETE FROM w;
         CREATE TEMP TABLE IF NOT EXISTS gone(id INTEGER PRIMARY KEY, path TEXT, remote_id TEXT,
             is_dir INTEGER, size INTEGER);
         DELETE FROM gone;",
    )?;
    {
        let mut insert = tx.prepare_cached(
            "INSERT OR REPLACE INTO w(path, remote_id, parent, name, name_fold, is_dir, size, mtime,
                                      ext, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for row in &rows {
            let d = derive(row);
            insert.execute(params![
                row.path,
                row.remote_id,
                d.parent,
                d.name,
                d.name_fold,
                row.is_dir,
                d.size,
                d.mtime,
                d.ext,
                d.kind
            ])?;
        }
    }
    drop(rows);

    let (lo, hi) = under_range(scope.at);
    let before: i64 = tx.query_row(
        "SELECT count(*) FROM entries WHERE root = ?1 AND deleted_at IS NULL",
        [scope.root],
        |r| r.get(0),
    )?;
    let first_walk = before == 0;
    let mut log = ChangeLog::new("walk");

    // Rows the listing didn't have.
    tx.execute(
        "INSERT INTO gone(id, path, remote_id, is_dir, size)
         SELECT e.id, e.path, e.remote_id, e.is_dir, coalesce(t.bytes, e.size) FROM entries e
         LEFT JOIN folder_totals t ON t.path = e.path
         WHERE e.root = ?1 AND e.path >= ?2 AND e.path < ?3 AND e.deleted_at IS NULL
           AND (e.own_write_at IS NULL OR e.own_write_at < ?4)
           AND NOT EXISTS (SELECT 1 FROM w WHERE w.path = e.path)",
        params![scope.root, lo, hi, scope.started],
    )?;

    // Moves: a missing row whose id turns up at a path the index doesn't have yet.
    let moves: Vec<(i64, String, String, bool, Option<i64>)> = tx
        .prepare(
            "SELECT g.id, g.path, w.path, w.is_dir, w.size FROM gone g
             JOIN w ON w.remote_id = g.remote_id
             WHERE g.remote_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM entries x WHERE x.path = w.path)",
        )?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut moved_ids = HashSet::new();
    let mut taken = HashSet::new();
    {
        let mut update = tx.prepare_cached(
            "UPDATE entries SET path = w.path, parent = w.parent, name = w.name,
                    name_fold = w.name_fold, is_dir = w.is_dir, size = w.size, mtime = w.mtime,
                    ext = w.ext, kind = w.kind, remote_id = w.remote_id, root = ?3, gen = ?4,
                    own_write_at = NULL
             FROM w WHERE entries.id = ?1 AND w.path = ?2",
        )?;
        for (id, old, new, is_dir, size) in &moves {
            if !moved_ids.insert(*id) || !taken.insert(new.clone()) {
                continue;
            }
            update.execute(params![id, new, scope.root, generation_no])?;
            log.push("moved", new, Some(old), *size, *is_dir);
        }
    }
    for id in &moved_ids {
        tx.execute("DELETE FROM gone WHERE id = ?1", [id])?;
    }

    // Held removals of entries this walk saw again: the folder walked and its ancestors
    // (listing it worked), and everything it listed (moved rows included). Confirming the
    // hold later must not remove what exists.
    tx.execute(
        "DELETE FROM held WHERE root = ?1 AND EXISTS
           (SELECT 1 FROM entries e JOIN w ON w.path = e.path WHERE e.id = held.entry_id)",
        [scope.root],
    )?;
    {
        let mut unhold = tx.prepare_cached(
            "DELETE FROM held WHERE root = ?1
               AND entry_id IN (SELECT id FROM entries WHERE path = ?2)",
        )?;
        let mut folder = scope.at;
        loop {
            unhold.execute(params![scope.root, folder])?;
            if folder == "/" || folder == scope.root {
                break;
            }
            folder = parent_of(folder);
        }
    }

    // Removals, unless the guard holds them.
    let removals: i64 = tx.query_row("SELECT count(*) FROM gone", [], |r| r.get(0))?;
    let removals = removals as u64;
    let limit = (scope.max_removal * before as f64).floor() as u64;
    let hold = !scope.confirmed && removals > limit.max(MIN_HELD_REMOVALS - 1);
    let mut applied = Applied {
        moved: moved_ids.len() as u64,
        ..Applied::default()
    };
    if hold {
        if scope.full() {
            tx.execute("DELETE FROM held WHERE root = ?1", [scope.root])?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO held(root, entry_id, gen) SELECT ?1, id, ?2 FROM gone",
            params![scope.root, generation_no],
        )?;
        let held: i64 = tx.query_row(
            "SELECT count(*) FROM held WHERE root = ?1",
            [scope.root],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE roots SET pending_removals = ?2, pending_at = ?3, pending_of = ?4 WHERE path = ?1",
            params![scope.root, held, scope.started, before],
        )?;
        applied.held = removals;
    } else {
        let gone: Vec<(String, bool, Option<i64>)> = tx
            .prepare("SELECT path, is_dir, size FROM gone")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        for (path, is_dir, size) in &gone {
            log.push("removed", path, None, *size, *is_dir);
        }
        tx.execute("DELETE FROM entries WHERE id IN (SELECT id FROM gone)", [])?;
        tx.execute(
            "DELETE FROM held WHERE entry_id IN (SELECT id FROM gone)",
            [],
        )?;
        if scope.full() {
            tx.execute("DELETE FROM held WHERE root = ?1", [scope.root])?;
        }
        sync_pending(tx, scope.root)?;
        applied.removed = removals;
    }

    // Additions.
    if !first_walk {
        let added: Vec<(String, bool, Option<i64>)> = tx
            .prepare(
                "SELECT w.path, w.is_dir, w.size FROM w
                 WHERE NOT EXISTS (SELECT 1 FROM entries e WHERE e.path = w.path)",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        for (path, is_dir, size) in &added {
            log.push("added", path, None, *size, *is_dir);
        }
    }
    applied.added = tx.execute(
        "INSERT INTO entries(remote_id, path, parent, name, name_fold, is_dir, size, mtime, ext,
                             kind, root, gen)
         SELECT remote_id, path, parent, name, name_fold, is_dir, size, mtime, ext, kind, ?1, ?2
         FROM w WHERE NOT EXISTS (SELECT 1 FROM entries e WHERE e.path = w.path)",
        params![scope.root, generation_no],
    )? as u64;

    // Rows that differ, and tombstones older than the walk (which brings the entry back).
    struct Differing {
        id: i64,
        path: String,
        tombstone: bool,
        was_dir: bool,
        old_size: Option<i64>,
        old_mtime: i64,
        is_dir: bool,
        size: Option<i64>,
        mtime: i64,
    }
    let differing: Vec<Differing> = tx
        .prepare(
            "SELECT e.id, e.path, e.deleted_at IS NOT NULL, e.is_dir, e.size, e.mtime,
                    w.is_dir, w.size, w.mtime
             FROM w JOIN entries e ON e.path = w.path
             WHERE (e.own_write_at IS NULL OR e.own_write_at < ?2)
               AND ((e.deleted_at IS NOT NULL AND e.deleted_at < ?2)
                 OR (e.deleted_at IS NULL AND (e.is_dir IS NOT w.is_dir OR e.size IS NOT w.size
                     OR e.mtime IS NOT w.mtime OR e.remote_id IS NOT w.remote_id
                     OR e.root IS NOT ?1)))",
        )?
        .query_map(params![scope.root, scope.started], |r| {
            Ok(Differing {
                id: r.get(0)?,
                path: r.get(1)?,
                tombstone: r.get(2)?,
                was_dir: r.get(3)?,
                old_size: r.get(4)?,
                old_mtime: r.get(5)?,
                is_dir: r.get(6)?,
                size: r.get(7)?,
                mtime: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    {
        let mut update = tx.prepare_cached(
            "UPDATE entries SET remote_id = w.remote_id, is_dir = w.is_dir, size = w.size,
                    mtime = w.mtime, ext = w.ext, kind = w.kind, root = ?2, gen = ?3,
                    deleted_at = NULL, own_write_at = NULL
             FROM w WHERE entries.id = ?1 AND w.path = entries.path",
        )?;
        for d in &differing {
            update.execute(params![d.id, scope.root, generation_no])?;
            if d.tombstone {
                log.push("added", &d.path, None, d.size, d.is_dir);
                applied.added += 1;
            } else if !d.is_dir && (d.old_size != d.size || d.old_mtime != d.mtime || d.was_dir) {
                log.push("changed", &d.path, None, d.size, false);
                applied.changed += 1;
            }
        }
    }

    // Tombstones the walk confirms (it started after the delete's second and didn't see the
    // entry).
    tx.execute(
        "DELETE FROM entries WHERE root = ?1 AND path >= ?2 AND path < ?3
           AND deleted_at IS NOT NULL AND deleted_at < ?4
           AND NOT EXISTS (SELECT 1 FROM w WHERE w.path = entries.path)",
        params![scope.root, lo, hi, scope.started],
    )?;

    log.write(tx)?;
    tx.execute_batch("DELETE FROM w; DELETE FROM gone;")?;
    let after: i64 = tx.query_row(
        "SELECT count(*) FROM entries WHERE root = ?1 AND deleted_at IS NULL",
        [scope.root],
        |r| r.get(0),
    )?;
    applied.entries = after as u64;
    Ok(applied)
}

/// Sets the root's pending removal count to the rows still held for it, clearing the
/// pending columns when none are left.
fn sync_pending(tx: &Transaction<'_>, root: &str) -> rusqlite::Result<()> {
    let held: i64 = tx.query_row("SELECT count(*) FROM held WHERE root = ?1", [root], |r| {
        r.get(0)
    })?;
    if held == 0 {
        tx.execute(
            "UPDATE roots SET pending_removals = NULL, pending_at = NULL, pending_of = NULL
             WHERE path = ?1",
            [root],
        )?;
    } else {
        tx.execute(
            "UPDATE roots SET pending_removals = ?2 WHERE path = ?1",
            params![root, held],
        )?;
    }
    Ok(())
}

#[derive(Default, Clone, Copy)]
struct Sum {
    files: i64,
    dirs: i64,
    bytes: i64,
    newest: i64,
}

/// Rebuilds `folder_totals` from every live entry: each file and folder counts towards all
/// its ancestors, `/` included.
pub(crate) fn recompute_totals(conn: &Connection) -> rusqlite::Result<()> {
    let mut sums: HashMap<String, Sum> = HashMap::new();
    sums.insert("/".into(), Sum::default());
    {
        let mut stmt =
            conn.prepare("SELECT path, is_dir, size, mtime FROM entries WHERE deleted_at IS NULL")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let is_dir: bool = row.get(1)?;
            let size: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
            let mtime: i64 = row.get(3)?;
            if is_dir && !sums.contains_key(path.as_str()) {
                sums.insert(path.clone(), Sum::default());
            }
            let mut folder = parent_of(&path);
            loop {
                let sum = match sums.get_mut(folder) {
                    Some(sum) => sum,
                    None => sums.entry(folder.to_owned()).or_default(),
                };
                if is_dir {
                    sum.dirs += 1;
                } else {
                    sum.files += 1;
                    sum.bytes += size;
                }
                sum.newest = sum.newest.max(mtime);
                if folder == "/" {
                    break;
                }
                folder = parent_of(folder);
            }
        }
    }
    conn.execute("DELETE FROM folder_totals", [])?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO folder_totals(path, files, dirs, bytes, newest) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (path, sum) in &sums {
        insert.execute(params![
            path,
            sum.files,
            sum.dirs,
            sum.bytes,
            (sum.newest > 0).then_some(sum.newest)
        ])?;
    }
    Ok(())
}

fn prune_changes(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM changes WHERE at < ?1",
        [time::now() - CHANGES_KEEP_S],
    )?;
    conn.execute(
        "DELETE FROM changes WHERE seq <= (SELECT max(seq) FROM changes) - ?1",
        [CHANGES_KEEP_ROWS],
    )?;
    Ok(())
}
