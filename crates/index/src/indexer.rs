//! The background side: scheduled and requested walks (one at a time), and hashing size
//! collisions while nothing else runs.
//!
//! A walk of a root lists it with every child root and hidden path split out (see
//! [`chance_of_files_core::split_walk`]), so each entry belongs to exactly one root and hidden
//! subtrees are never fetched. Applying it:
//!
//! - adds, size/mtime changes and moves (same remote id, new path) are applied and logged;
//! - rows not seen are removed, except rows our own writes touched after the walk started;
//! - tombstones of our own deletes newer than the walk keep the entry out; older ones are
//!   dropped (or the entry comes back, if the walk saw it);
//! - a walk that would remove more than `max_removal` of the root's entries applies
//!   everything else and holds the removals until `refresh_index confirm=true`; any later
//!   walk releases the held rows it sees again, and confirming skips rows seen or written
//!   since the hold;
//! - a walk that started but never finished (the process stopped) is retried after the
//!   first back-off when the index opens again;
//! - a failed walk changes nothing but `last_error`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chance_of_files_core::{
    CheckedPath, Health, IndexedEntry, Policy, Storage, StorageError, fold, time,
};
use futures_util::StreamExt;
use tokio::sync::Notify;

use crate::apply::{WalkRow, WalkScope};
use crate::db::{Db, lock};
use crate::query;

/// One root to index and how often.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootConfig {
    pub path: String,
    pub every: Duration,
}

#[derive(Debug, Clone)]
pub struct IndexConfig {
    /// Where `index.db` lives.
    pub dir: std::path::PathBuf,
    /// Non-overlapping in effect: a parent root's walk skips its child roots.
    pub roots: Vec<RootConfig>,
    /// Share of a root's entries one walk may remove before the removals are held.
    pub max_removal: f64,
    /// Smallest file whose size collisions get hashed.
    pub hash_min_size: u64,
    /// Hash collisions in the background after walks.
    pub hash_in_background: bool,
    /// Hash calls at once (each is one pCloud API call, under rclone's --tpslimit).
    pub hash_concurrency: usize,
    /// A walk taking longer than this counts as failed.
    pub walk_timeout: Duration,
}

impl IndexConfig {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            roots: vec![RootConfig {
                path: "/".into(),
                every: Duration::from_secs(30 * 60),
            }],
            max_removal: 0.05,
            hash_min_size: 1024 * 1024,
            hash_in_background: true,
            hash_concurrency: 4,
            walk_timeout: Duration::from_secs(30 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Job {
    Full(String),
    Subtree { root: String, at: String },
}

impl Job {
    pub(crate) fn root(&self) -> &str {
        match self {
            Self::Full(root) | Self::Subtree { root, .. } => root,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Attempt {
    at: i64,
    failures: u32,
}

#[derive(Debug, Default)]
pub(crate) struct State {
    pub(crate) queue: VecDeque<Job>,
    pub(crate) walking: Option<Job>,
    attempts: HashMap<String, Attempt>,
}

pub(crate) struct Shared {
    pub(crate) db: Db,
    pub(crate) config: IndexConfig,
    pub(crate) policy: Policy,
    pub(crate) health: Arc<Health>,
    pub(crate) state: Mutex<State>,
    pub(crate) wake: Notify,
}

impl Shared {
    /// The root an entry at `path` belongs to: the deepest root strictly above it (a child
    /// root's own folder entry belongs to its parent), or `path` itself when it is a root
    /// with no root above.
    pub(crate) fn root_holding(&self, path: &str) -> Option<&str> {
        let folded = fold(path);
        self.config
            .roots
            .iter()
            .filter(|r| {
                let root = fold(&r.path);
                root == "/"
                    || (folded.len() > root.len()
                        && folded.starts_with(&root)
                        && folded.as_bytes()[root.len()] == b'/')
            })
            .max_by_key(|r| r.path.len())
            .map(|r| r.path.as_str())
    }

    /// The root that is `path` or the deepest one above it: whose walk shows `path`'s contents.
    pub(crate) fn root_covering(&self, path: &str) -> Option<&str> {
        let folded = fold(path);
        self.config
            .roots
            .iter()
            .find(|r| fold(&r.path) == folded)
            .map(|r| r.path.as_str())
            .or_else(|| self.root_holding(path))
    }

    pub(crate) fn interval(&self, root: &str) -> Duration {
        self.config
            .roots
            .iter()
            .find(|r| r.path == root)
            .map_or(Duration::from_secs(1800), |r| r.every)
    }

    /// When a root's next scheduled walk is due (Unix seconds). Failed walks retry sooner:
    /// after 1, 2, 4... minutes, never later than the root's interval.
    pub(crate) fn due(&self, state: &State, root: &str) -> i64 {
        let every = self.interval(root).as_secs() as i64;
        match state.attempts.get(root) {
            None => 0,
            Some(a) if a.failures == 0 => a.at + every,
            Some(a) => a.at + every.min(60i64 << a.failures.min(10)),
        }
    }

    /// Seeds the schedule from the last walks recorded in the file. A full walk that started
    /// and never finished (the process stopped mid-walk) counts as a failure, so it is retried
    /// after the first back-off instead of a whole interval later.
    pub(crate) fn seed_attempts(&self) {
        let roots = {
            let Ok(conn) = self.db.reader() else {
                return;
            };
            let Ok(mut stmt) = conn.prepare("SELECT path, walk_started, last_ok FROM roots") else {
                return;
            };
            let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            }) else {
                return;
            };
            rows.flatten().collect::<Vec<_>>()
        };
        let mut state = lock(&self.state);
        for (root, started, last_ok) in roots {
            if let Some(at) = started {
                let interrupted = at > last_ok.unwrap_or(0);
                state.attempts.insert(
                    root,
                    Attempt {
                        at,
                        failures: u32::from(interrupted),
                    },
                );
            }
        }
    }

    /// Queues a job unless the same one is queued already; returns whether it was new.
    pub(crate) fn enqueue(&self, job: Job) -> bool {
        let mut state = lock(&self.state);
        if state.queue.contains(&job) {
            return false;
        }
        state.queue.push_back(job);
        drop(state);
        self.wake.notify_one();
        true
    }
}

/// Runs the walks and the hashing. Create with [`crate::open`]; drive with [`Indexer::run`].
pub struct Indexer<S> {
    pub(crate) shared: Arc<Shared>,
    pub(crate) storage: S,
}

/// Runs blocking database work off the async threads.
async fn blocking<T: Send + 'static>(
    shared: &Arc<Shared>,
    work: impl FnOnce(&Shared) -> T + Send + 'static,
) -> T {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || work(&shared))
        .await
        .expect("index work doesn't panic")
}

impl<S: Storage> Indexer<S> {
    /// Forever: requested walks first, then due roots, then hashing; sleeps when idle.
    pub async fn run(self) {
        loop {
            if !self.shared.health.is_healthy() {
                // Walks pause while the backend is down; the health check clears this.
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(30)) => {},
                    () = self.shared.wake.notified() => {},
                }
                continue;
            }
            if let Some(job) = self.next_job() {
                self.walk(job).await;
                continue;
            }
            if self.shared.config.hash_in_background && self.hash_batch().await > 0 {
                continue;
            }
            let wait = self.until_next_due();
            tokio::select! {
                () = tokio::time::sleep(wait) => {},
                () = self.shared.wake.notified() => {},
            }
        }
    }

    fn next_job(&self) -> Option<Job> {
        let mut state = lock(&self.shared.state);
        if let Some(job) = state.queue.pop_front() {
            return Some(job);
        }
        let now = time::now();
        let due = self
            .shared
            .config
            .roots
            .iter()
            .map(|r| (self.shared.due(&state, &r.path), &r.path))
            .filter(|(due, _)| *due <= now)
            .min()?;
        Some(Job::Full(due.1.clone()))
    }

    fn until_next_due(&self) -> Duration {
        let state = lock(&self.shared.state);
        let now = time::now();
        let next = self
            .shared
            .config
            .roots
            .iter()
            .map(|r| self.shared.due(&state, &r.path))
            .min()
            .unwrap_or(now + 60);
        Duration::from_secs((next - now).clamp(1, 60) as u64)
    }

    /// Lists and applies one job. Never fails: errors are recorded on the root.
    pub async fn walk_now(&self, job_root: &str, at: Option<&str>) {
        let job = match at {
            Some(at) if at != job_root => Job::Subtree {
                root: job_root.to_owned(),
                at: at.to_owned(),
            },
            _ => Job::Full(job_root.to_owned()),
        };
        self.walk(job).await;
    }

    async fn walk(&self, job: Job) {
        let shared = &self.shared;
        let root = job.root().to_owned();
        let started = time::now();
        let clock = Instant::now();
        {
            let mut state = lock(&shared.state);
            state.walking = Some(job.clone());
            let failures = state.attempts.get(&root).map_or(0, |a| a.failures);
            if matches!(job, Job::Full(_)) {
                state.attempts.insert(
                    root.clone(),
                    Attempt {
                        at: started,
                        failures,
                    },
                );
            }
        }
        let full = matches!(job, Job::Full(_));
        let at = match &job {
            Job::Full(root) => root.clone(),
            Job::Subtree { at, .. } => at.clone(),
        };
        let at = {
            let wanted = at.clone();
            blocking(shared, move |s| {
                s.db.reader()
                    .ok()
                    .and_then(|c| query::stored_path(&c, &wanted).ok().flatten())
                    .unwrap_or(wanted)
            })
            .await
        };
        if full {
            let (r, t) = (root.clone(), started);
            if let Err(e) = blocking(shared, move |s| s.db.walk_starting(&r, t)).await {
                tracing::warn!(root = %root, error = %e, "can't record the walk start");
            }
        }

        let result = self.list(&at).await;
        let outcome = match result {
            Ok(rows) => {
                let listed = rows.len();
                let list_ms = clock.elapsed().as_millis() as u64;
                let (r, a) = (root.clone(), at.clone());
                let max_removal = shared.config.max_removal;
                let applied = blocking(shared, move |s| {
                    let scope = WalkScope {
                        root: &r,
                        at: &a,
                        started,
                        max_removal,
                        confirmed: false,
                    };
                    let took = clock.elapsed().as_millis() as u64;
                    s.db.apply_walk(&scope, rows, took)
                })
                .await;
                match applied {
                    Ok(applied) => {
                        tracing::info!(
                            root = %root, at = %at, listed, list_ms,
                            ms = clock.elapsed().as_millis() as u64,
                            added = applied.added, removed = applied.removed,
                            changed = applied.changed, moved = applied.moved,
                            held = applied.held, entries = applied.entries,
                            "walk applied"
                        );
                        if applied.held > 0 {
                            tracing::warn!(root = %root, held = applied.held, "walk would remove too much; removals held until refresh_index confirm=true");
                        }
                        Ok(())
                    }
                    Err(e) => Err(format!("applying the walk failed: {e}")),
                }
            }
            Err(err) => {
                if matches!(err, StorageError::Backend(_) | StorageError::Unhealthy(_)) {
                    shared.health.poke();
                }
                Err(err.to_string())
            }
        };
        {
            let mut state = lock(&shared.state);
            state.walking = None;
            if full {
                let attempt = state.attempts.entry(root.clone()).or_default();
                attempt.failures = if outcome.is_ok() {
                    0
                } else {
                    attempt.failures + 1
                };
            }
        }
        if let Err(error) = outcome {
            tracing::warn!(root = %root, at = %at, error = %error, "walk failed");
            let _ = blocking(shared, move |s| s.db.walk_failed(&root, &error)).await;
        }
    }

    /// Lists `at` with child roots and hidden paths split out, keeping visible entries.
    async fn list(&self, at: &str) -> Result<Vec<WalkRow>, StorageError> {
        let shared = &self.shared;
        let checked = shared.policy.check(at)?;
        // Exclusions are never fetched, so they need no policy check of their own.
        let any = Policy::default();
        let folded_at = fold(at);
        let mut exclude: Vec<CheckedPath> = Vec::new();
        for root in &shared.config.roots {
            let folded = fold(&root.path);
            let below = folded != folded_at
                && (folded_at == "/" || folded.starts_with(&format!("{folded_at}/")));
            if below {
                exclude.push(any.check(&root.path)?);
            }
        }
        for hidden in shared.policy.hidden().paths() {
            exclude.push(any.check(hidden)?);
        }
        let mut rows = Vec::new();
        let policy = &shared.policy;
        let mut keep = |entry: chance_of_files_core::Entry| {
            if policy.visible(&entry.path) {
                rows.push(WalkRow::from(entry));
            }
        };
        let walk = self.storage.walk_each(&checked, &exclude, &mut keep);
        match tokio::time::timeout(shared.config.walk_timeout, walk).await {
            Ok(Ok(())) => Ok(rows),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(StorageError::Backend(format!(
                "the walk of {at} took longer than {} s",
                shared.config.walk_timeout.as_secs()
            ))),
        }
    }

    /// Hashes a batch of unhashed size-collision files; returns how many were looked at.
    async fn hash_batch(&self) -> usize {
        let min = self.shared.config.hash_min_size;
        let todo = blocking(&self.shared, move |s| {
            s.db.reader()
                .map_err(query::db_error)
                .and_then(|c| query::unhashed(&c, min, 32))
        })
        .await;
        let todo = match todo {
            Ok(todo) => todo,
            Err(e) => {
                tracing::warn!(error = %e, "can't list files to hash");
                return 0;
            }
        };
        if todo.is_empty() {
            return 0;
        }
        let results = hash_entries(
            &self.storage,
            &self.shared.policy,
            todo,
            self.shared.config.hash_concurrency,
        )
        .await;
        let mut done = 0;
        let mut stop = false;
        for (entry, result) in results {
            let hashes = match result {
                Ok(hashes) => hashes,
                Err(StorageError::NotFound(_)) => Vec::new(),
                Err(err) => {
                    tracing::warn!(error = %err, "hashing stopped");
                    self.shared.health.poke();
                    stop = true;
                    continue;
                }
            };
            done += 1;
            let _ = blocking(&self.shared, move |s| {
                query::store_hashes(&s.db.writer(), &entry, &hashes)
            })
            .await;
        }
        if stop { 0 } else { done }
    }
}

/// Fetches hashes for entries, `concurrency` at a time. A file whose size or mtime changed
/// since the index saw it gets no hashes (an empty result marks it as tried until the next
/// walk updates it).
pub async fn hash_entries<S: Storage>(
    storage: &S,
    policy: &Policy,
    entries: Vec<IndexedEntry>,
    concurrency: usize,
) -> Vec<(
    IndexedEntry,
    Result<Vec<(chance_of_files_core::HashKind, String)>, StorageError>,
)> {
    futures_util::stream::iter(entries)
        .map(|entry| async move {
            let result = async {
                let checked = policy.check(&entry.path)?;
                let live = storage.stat(&checked, true).await?;
                let mtime = live.modified.as_deref().and_then(time::parse_rfc3339);
                if live.size != entry.size || (entry.mtime.is_some() && mtime != entry.mtime) {
                    return Ok(Vec::new());
                }
                Ok(live.hashes)
            }
            .await;
            (entry, result)
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await
}
