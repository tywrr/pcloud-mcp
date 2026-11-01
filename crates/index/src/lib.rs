//! The metadata index of Chance of Files: every visible entry of the configured roots in
//! SQLite (`index.db`), kept current by scheduled split walks, with folder totals, a change
//! log and cached hashes, behind [`chance_of_files_core::Index`].
//!
//! ```text
//! let (index, indexer) = chance_of_files_index::open(config, policy, health, storage)?;
//! tokio::spawn(indexer.run());          // walks and hashing, one thing at a time
//! McpServer::new(..).with_index(index); // the tools read through the handle
//! ```

mod apply;
mod db;
mod indexer;
mod paths;
mod query;

use std::sync::{Arc, Mutex};

use chance_of_files_core::{
    Change, Filter, HashKind, Health, HeldRemoval, Index, IndexError, IndexResult, IndexStatus,
    IndexedEntry, Policy, RootStatus, SizeGroups, Sort, Storage, TypeRow, fold, time,
};
use tokio::sync::Notify;

pub use apply::{Applied, WalkRow, WalkScope};
pub use db::Db;
pub use indexer::{IndexConfig, Indexer, RootConfig, hash_entries};

use crate::db::lock;
use crate::indexer::{Job, Shared, State};

/// Opens the index and makes its two halves: the handle the tools read through, and the
/// indexer to run in the background. Roots are checked against the policy (none may be
/// hidden, none repeated); stored rows are brought in line with the roots and the policy.
pub fn open<S: Storage>(
    mut config: IndexConfig,
    policy: Policy,
    health: Arc<Health>,
    storage: S,
) -> Result<(Arc<IndexHandle>, Indexer<S>), String> {
    let mut seen = Vec::new();
    for root in &mut config.roots {
        let checked = policy
            .check(&root.path)
            .map_err(|_| format!("index root {} is hidden or not a path", root.path))?;
        root.path = checked.as_str().to_owned();
        let folded = fold(&root.path);
        if seen.contains(&folded) {
            return Err(format!("index root {} is listed twice", root.path));
        }
        seen.push(folded);
        if root.every < std::time::Duration::from_secs(60) {
            return Err(format!(
                "index root {}: walk at most once a minute",
                root.path
            ));
        }
    }
    if !(0.0..=1.0).contains(&config.max_removal) {
        return Err("index max_removal must be between 0 and 1".into());
    }
    let db = Db::open(&config.dir)?;
    let roots: Vec<(String, u64)> = config
        .roots
        .iter()
        .map(|r| (r.path.clone(), r.every.as_secs()))
        .collect();
    let fixed = db
        .reconcile(&roots, &policy)
        .map_err(|e| format!("index: {e}"))?;
    if fixed > 0 {
        tracing::info!(
            rows = fixed,
            "index rows moved to their roots or dropped as hidden"
        );
    }
    let shared = Arc::new(Shared {
        db,
        config,
        policy,
        health,
        state: Mutex::new(State::default()),
        wake: Notify::new(),
    });
    shared.seed_attempts();
    let handle = Arc::new(IndexHandle {
        shared: shared.clone(),
    });
    Ok((handle, Indexer { shared, storage }))
}

/// What the tools hold: reads, and requests for walks.
pub struct IndexHandle {
    shared: Arc<Shared>,
}

impl IndexHandle {
    pub fn db(&self) -> &Db {
        &self.shared.db
    }

    fn reader(&self) -> IndexResult<db::Reader<'_>> {
        self.shared.db.reader().map_err(query::db_error)
    }

    /// Records a confirmed write of our own (phase 3's write tools call this).
    pub fn record_own_write(&self, row: &WalkRow) -> IndexResult<()> {
        let root = self
            .shared
            .root_holding(&row.path)
            .ok_or_else(|| {
                IndexError::Invalid(format!("{} is outside every index root", row.path))
            })?
            .to_owned();
        self.shared
            .db
            .record_own_write(&root, row, time::now())
            .map_err(query::db_error)
    }

    /// Records a confirmed delete of our own (to the trash).
    pub fn record_own_delete(&self, path: &str) -> IndexResult<u64> {
        self.shared
            .db
            .record_own_delete(path, time::now())
            .map_err(query::db_error)
    }

    fn root_status(&self, rows: Vec<RootRow>) -> Vec<RootStatus> {
        let state = lock(&self.shared.state);
        rows.into_iter()
            .map(|row| {
                let walking = state.walking.as_ref().is_some_and(|j| j.root() == row.path);
                let queued = state.queue.iter().any(|j| j.root() == row.path);
                let next_due = Some(self.shared.due(&state, &row.path).max(time::now()));
                RootStatus {
                    every_s: row.every_s,
                    walk_started: row.walk_started,
                    as_of: row.last_ok,
                    last_ms: row.last_ms,
                    last_error: row.last_error,
                    entries: row.entries,
                    held: row.held,
                    walking,
                    queued,
                    next_due,
                    path: row.path,
                }
            })
            .collect()
    }
}

struct RootRow {
    path: String,
    every_s: u64,
    walk_started: Option<i64>,
    last_ok: Option<i64>,
    last_ms: Option<u64>,
    last_error: Option<String>,
    entries: u64,
    held: Option<HeldRemoval>,
}

fn root_rows(conn: &rusqlite::Connection, order: &[String]) -> IndexResult<Vec<RootRow>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT path, every_s, walk_started, last_ok, last_ms, last_error, entries,
                    pending_removals, pending_at, pending_of FROM roots",
        )
        .map_err(query::db_error)?;
    let mut rows: Vec<RootRow> = stmt
        .query_map([], |r| {
            let pending: Option<i64> = r.get(7)?;
            Ok(RootRow {
                path: r.get(0)?,
                every_s: r.get::<_, i64>(1)? as u64,
                walk_started: r.get(2)?,
                last_ok: r.get(3)?,
                last_ms: r.get::<_, Option<i64>>(4)?.map(|m| m as u64),
                last_error: r.get(5)?,
                entries: r.get::<_, i64>(6)? as u64,
                held: match pending {
                    Some(n) if n > 0 => Some(HeldRemoval {
                        at: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                        removals: n as u64,
                        entries: r.get::<_, Option<i64>>(9)?.unwrap_or(0) as u64,
                    }),
                    _ => None,
                },
            })
        })
        .map_err(query::db_error)?
        .collect::<Result<_, _>>()
        .map_err(query::db_error)?;
    rows.sort_by_key(|r| {
        order
            .iter()
            .position(|p| *p == r.path)
            .unwrap_or(usize::MAX)
    });
    Ok(rows)
}

impl Index for IndexHandle {
    fn roots(&self) -> IndexResult<Vec<RootStatus>> {
        let order: Vec<String> = self
            .shared
            .config
            .roots
            .iter()
            .map(|r| r.path.clone())
            .collect();
        let rows = root_rows(&*self.reader()?, &order)?;
        Ok(self.root_status(rows))
    }

    fn status(&self) -> IndexResult<IndexStatus> {
        let roots = self.roots()?;
        let conn = self.reader()?;
        let min = self.shared.config.hash_min_size;
        let (hash_candidates, hashed) = query::hash_progress(&conn, min)?;
        Ok(IndexStatus {
            roots,
            generation: query::generation(&conn)? as u64,
            db_bytes: self.shared.db.bytes(),
            hash_candidates,
            hashed,
            hash_min_size: min,
        })
    }

    fn entry(&self, path: &str) -> IndexResult<Option<IndexedEntry>> {
        query::entry(&*self.reader()?, path)
    }

    fn children(
        &self,
        folder: &str,
        sort: Sort,
        cursor: Option<&str>,
        limit: usize,
    ) -> IndexResult<Vec<IndexedEntry>> {
        query::children(
            &*self.reader()?,
            &self.shared.policy,
            folder,
            sort,
            cursor,
            limit,
        )
    }

    fn child_counts(&self, folder: &str) -> IndexResult<(u64, u64)> {
        query::child_counts(&*self.reader()?, folder)
    }

    fn find(
        &self,
        filter: &Filter,
        sort: Sort,
        cursor: Option<&str>,
        limit: usize,
    ) -> IndexResult<Vec<IndexedEntry>> {
        query::find(
            &*self.reader()?,
            &self.shared.policy,
            filter,
            sort,
            cursor,
            limit,
        )
    }

    fn count(&self, filter: &Filter) -> IndexResult<(u64, u64)> {
        query::count(&*self.reader()?, &self.shared.policy, filter)
    }

    fn folders(
        &self,
        under: &str,
        depth: u32,
        sort: Sort,
        cursor: Option<&str>,
        limit: usize,
    ) -> IndexResult<Vec<IndexedEntry>> {
        query::folders(
            &*self.reader()?,
            &self.shared.policy,
            under,
            depth,
            sort,
            cursor,
            limit,
        )
    }

    fn types(&self, under: &str) -> IndexResult<Vec<TypeRow>> {
        query::types(&*self.reader()?, under)
    }

    fn changes(
        &self,
        under: &str,
        since: i64,
        source: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> IndexResult<Vec<Change>> {
        query::changes(
            &*self.reader()?,
            &self.shared.policy,
            under,
            since,
            source,
            cursor,
            limit,
        )
    }

    fn size_groups(
        &self,
        under: &str,
        min_size: u64,
        cursor: Option<&str>,
        max_files: usize,
    ) -> IndexResult<SizeGroups> {
        query::size_groups(
            &*self.reader()?,
            &self.shared.policy,
            under,
            min_size,
            cursor,
            max_files,
        )
    }

    fn store_hashes(&self, entry: &IndexedEntry, hashes: &[(HashKind, String)]) -> IndexResult<()> {
        query::store_hashes(&self.shared.db.writer(), entry, hashes)
    }

    fn refresh(&self, path: Option<&str>, confirm: bool) -> IndexResult<String> {
        let shared = &self.shared;
        let targets: Vec<(String, Option<String>)> = match path {
            None => shared
                .config
                .roots
                .iter()
                .map(|r| (r.path.clone(), None))
                .collect(),
            Some(path) => {
                let root = shared.root_covering(path).ok_or_else(|| {
                    IndexError::Invalid(format!(
                        "{path} is not inside any indexed root; the roots are {}.",
                        shared
                            .config
                            .roots
                            .iter()
                            .map(|r| r.path.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
                let at = if fold(root) == fold(path) {
                    None
                } else {
                    // A folder below the root gets a walk of its own; anything else (a file,
                    // or a path the index doesn't know) re-walks the root.
                    let conn = self.reader()?;
                    query::entry(&conn, path)?
                        .filter(|e| e.is_dir)
                        .map(|e| e.path)
                };
                vec![(root.to_owned(), at)]
            }
        };
        let mut lines = Vec::new();
        if confirm {
            let order: Vec<String> = targets.iter().map(|(r, _)| r.clone()).collect();
            let rows = root_rows(&*self.reader()?, &order)?;
            for row in rows.iter().filter(|r| order.contains(&r.path)) {
                if row.held.is_some() {
                    let removed = shared.db.apply_held(&row.path).map_err(query::db_error)?;
                    lines.push(format!(
                        "Applied the held removal of {removed} entries from {}.",
                        row.path
                    ));
                }
            }
            if lines.is_empty() {
                lines.push("No held removals to confirm.".into());
            }
        }
        for (root, at) in targets {
            let job = match &at {
                Some(at) => Job::Subtree {
                    root: root.clone(),
                    at: at.clone(),
                },
                None => Job::Full(root.clone()),
            };
            let what = match &at {
                Some(at) => format!("{at} (in root {root})"),
                None => root.clone(),
            };
            let walking = lock(&shared.state).walking.as_ref() == Some(&job);
            if shared.enqueue(job) {
                let note = if walking {
                    "; a walk of it is running now, this one follows"
                } else {
                    ""
                };
                lines.push(format!("Queued a walk of {what}{note}."));
            } else {
                lines.push(format!("A walk of {what} is already queued."));
            }
        }
        if !shared.health.is_healthy() {
            lines.push("The backend is unhealthy, so walks wait until it recovers.".into());
        }
        lines.push("index_status shows when it is done.".into());
        Ok(lines.join(" "))
    }
}
