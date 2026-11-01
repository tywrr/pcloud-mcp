//! `state.db`: what can't be rebuilt from pCloud (jobs, links and their use, the audit log).
//! Small, so one connection behind a mutex. A file that fails SQLite's quick check is moved
//! aside and a fresh one started: links and running jobs are lost, the old file is kept.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use chance_of_files_core::{
    JobRow, JobStatus, LinkKind, LinkRow, LinkState, LinkUse, StateError, StateResult, StateStore,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

const SCHEMA_VERSION: i64 = 1;
/// Link rows and their log are kept this long after expiry, for list_links.
const LINKS_KEEP_S: i64 = 30 * 86_400;
const JOBS_KEEP_S: i64 = 90 * 86_400;

const SCHEMA: &str = "
CREATE TABLE links(id TEXT PRIMARY KEY, kind TEXT NOT NULL, paths TEXT NOT NULL,
                   zip INTEGER NOT NULL, name TEXT NOT NULL, created INTEGER NOT NULL,
                   expires INTEGER NOT NULL, bound_net TEXT, size INTEGER NOT NULL,
                   max_bytes INTEGER NOT NULL, bytes_used INTEGER NOT NULL DEFAULT 0,
                   committed INTEGER NOT NULL DEFAULT 0, overwrite INTEGER NOT NULL DEFAULT 0,
                   state TEXT NOT NULL, note TEXT);
CREATE INDEX links_created ON links(created);
CREATE TABLE link_log(at INTEGER NOT NULL, link_id TEXT NOT NULL, ip TEXT NOT NULL,
                      range TEXT NOT NULL, bytes INTEGER NOT NULL, status INTEGER NOT NULL);
CREATE INDEX link_log_link ON link_log(link_id, at);
CREATE TABLE jobs(id TEXT PRIMARY KEY, rc_jobid TEXT NOT NULL, op TEXT NOT NULL,
                  src TEXT NOT NULL, dst TEXT, started INTEGER NOT NULL, finished INTEGER,
                  state TEXT NOT NULL, error TEXT);
CREATE INDEX jobs_started ON jobs(started);
CREATE TABLE audit(seq INTEGER PRIMARY KEY, at INTEGER NOT NULL, tool TEXT NOT NULL,
                   args TEXT NOT NULL, result TEXT NOT NULL);
";

pub struct State {
    path: PathBuf,
    conn: Mutex<Connection>,
    staging: PathBuf,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").field("path", &self.path).finish()
    }
}

/// What binding a link to a client's network found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bound {
    /// First use: bound now. Or a later use from the same network.
    Ok,
    /// Bound to another network.
    Elsewhere(String),
}

fn err(e: rusqlite::Error) -> StateError {
    StateError(e.to_string())
}

impl State {
    /// Opens (or creates) `state.db` in `dir`; `staging` is where upload parts live.
    pub fn open(dir: &Path, staging: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("can't create the state directory {}: {e}", dir.display()))?;
        std::fs::create_dir_all(staging).map_err(|e| {
            format!(
                "can't create the staging directory {}: {e}",
                staging.display()
            )
        })?;
        let path = dir.join("state.db");
        let conn = match open_checked(&path) {
            Ok(conn) => conn,
            Err(why) => {
                let aside = dir.join(format!(
                    "state.db.corrupt-{}",
                    chance_of_files_core::time::now()
                ));
                tracing::error!(error = %why, moved_to = %aside.display(), "state.db is unusable; starting a new one (links and running jobs are lost)");
                std::fs::rename(&path, &aside)
                    .map_err(|e| format!("can't move the broken state.db aside: {e}"))?;
                for suffix in ["-wal", "-shm"] {
                    let side = dir.join(format!("state.db{suffix}"));
                    let _ = std::fs::rename(&side, aside.with_extension(format!("db{suffix}")));
                }
                open_checked(&path)?
            }
        };
        Ok(Self {
            path,
            conn: Mutex::new(conn),
            staging: staging.to_owned(),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn staging(&self) -> &Path {
        &self.staging
    }

    /// The staging file of an upload link.
    pub fn part_file(&self, id: &str) -> PathBuf {
        self.staging.join(format!("{id}.part"))
    }

    /// Binds a link to `net` on first use; later uses must come from the same network.
    pub fn bind(&self, id: &str, net: &str) -> StateResult<Bound> {
        let conn = self.conn();
        conn.execute(
            "UPDATE links SET bound_net = ?2 WHERE id = ?1 AND bound_net IS NULL",
            params![id, net],
        )
        .map_err(err)?;
        let bound: Option<String> = conn
            .query_row("SELECT bound_net FROM links WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(err)?
            .flatten();
        Ok(match bound {
            Some(b) if b != net => Bound::Elsewhere(b),
            _ => Bound::Ok,
        })
    }

    /// Takes `bytes` from a link's allowance, unless that would pass its cap.
    pub fn take_bytes(&self, id: &str, bytes: u64) -> StateResult<bool> {
        let n = self
            .conn()
            .execute(
                "UPDATE links SET bytes_used = bytes_used + ?2
                 WHERE id = ?1 AND bytes_used + ?2 <= max_bytes",
                params![id, bytes as i64],
            )
            .map_err(err)?;
        Ok(n == 1)
    }

    /// Gives back bytes taken but not moved (a transfer that stopped early).
    pub fn give_back(&self, id: &str, bytes: u64) -> StateResult<()> {
        if bytes > 0 {
            self.conn()
                .execute(
                    "UPDATE links SET bytes_used = max(0, bytes_used - ?2) WHERE id = ?1",
                    params![id, bytes as i64],
                )
                .map_err(err)?;
        }
        Ok(())
    }

    pub fn log_use(&self, link_id: &str, u: &LinkUse) -> StateResult<()> {
        self.conn()
            .execute(
                "INSERT INTO link_log(at, link_id, ip, range, bytes, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![u.at, link_id, u.ip, u.range, u.bytes as i64, u.status],
            )
            .map_err(err)?;
        Ok(())
    }

    pub fn set_committed(&self, id: &str, committed: u64) -> StateResult<()> {
        self.conn()
            .execute(
                "UPDATE links SET committed = ?2 WHERE id = ?1",
                params![id, committed as i64],
            )
            .map_err(err)?;
        Ok(())
    }

    /// Upload links whose staging file should go: revoked, or past expiry without finishing.
    /// Expired ones are marked so. Old rows and log entries are pruned on the way.
    pub fn sweep(&self, now: i64) -> StateResult<Vec<String>> {
        let conn = self.conn();
        conn.execute(
            "UPDATE links SET state = 'expired'
             WHERE state IN ('active', 'failed') AND expires <= ?1",
            [now],
        )
        .map_err(err)?;
        let ids: Vec<String> = conn
            .prepare(
                "SELECT id FROM links WHERE kind = 'ul' AND state IN ('revoked', 'expired')
                   AND expires > ?1",
            )
            .map_err(err)?
            .query_map([now - 3 * 86_400], |r| r.get(0))
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)?;
        let cutoff = now - LINKS_KEEP_S;
        conn.execute(
            "DELETE FROM link_log WHERE link_id IN (SELECT id FROM links WHERE expires < ?1)",
            [cutoff],
        )
        .map_err(err)?;
        conn.execute("DELETE FROM links WHERE expires < ?1", [cutoff])
            .map_err(err)?;
        conn.execute(
            "DELETE FROM jobs WHERE state != 'running' AND started < ?1",
            [now - JOBS_KEEP_S],
        )
        .map_err(err)?;
        Ok(ids)
    }

    /// Uploads whose last step was interrupted (the process stopped while sending the file
    /// to pCloud).
    pub fn finishing(&self) -> StateResult<Vec<String>> {
        self.conn()
            .prepare("SELECT id FROM links WHERE kind = 'ul' AND state = 'finishing'")
            .map_err(err)?
            .query_map([], |r| r.get(0))
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)
    }
}

fn open_checked(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|e| e.to_string())?;
    // FULL: these rows can't be rebuilt, and writes are rare.
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
        .map_err(|e| e.to_string())?;
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if check != "ok" {
        return Err(format!("quick_check: {check}"));
    }
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    match version {
        SCHEMA_VERSION => {}
        0 => conn
            .execute_batch(&format!(
                "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))
            .map_err(|e| e.to_string())?,
        other => {
            return Err(format!(
                "state.db has schema version {other}; this version knows {SCHEMA_VERSION}"
            ));
        }
    }
    Ok(conn)
}

fn job_row(r: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: r.get(0)?,
        backend_id: r.get(1)?,
        op: r.get(2)?,
        src: r.get(3)?,
        dst: r.get(4)?,
        started: r.get(5)?,
        finished: r.get(6)?,
        state: JobStatus::parse(&r.get::<_, String>(7)?),
        error: r.get(8)?,
    })
}

const JOB_COLUMNS: &str = "id, rc_jobid, op, src, dst, started, finished, state, error";
const LINK_COLUMNS: &str = "id, kind, paths, zip, name, created, expires, bound_net, size,
                            max_bytes, bytes_used, committed, overwrite, state, note";

fn link_row(r: &Row<'_>) -> rusqlite::Result<LinkRow> {
    let paths: String = r.get(2)?;
    Ok(LinkRow {
        id: r.get(0)?,
        kind: LinkKind::parse(&r.get::<_, String>(1)?).unwrap_or(LinkKind::Download),
        paths: serde_json::from_str(&paths).unwrap_or_default(),
        zip: r.get(3)?,
        name: r.get(4)?,
        created: r.get(5)?,
        expires: r.get(6)?,
        bound_net: r.get(7)?,
        size: r.get::<_, i64>(8)? as u64,
        max_bytes: r.get::<_, i64>(9)? as u64,
        bytes_used: r.get::<_, i64>(10)? as u64,
        committed: r.get::<_, i64>(11)? as u64,
        overwrite: r.get(12)?,
        state: LinkState::parse(&r.get::<_, String>(13)?),
        note: r.get(14)?,
    })
}

impl StateStore for State {
    fn insert_job(&self, job: &JobRow) -> StateResult<()> {
        self.conn()
            .execute(
                &format!(
                    "INSERT INTO jobs({JOB_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                ),
                params![
                    job.id,
                    job.backend_id,
                    job.op,
                    job.src,
                    job.dst,
                    job.started,
                    job.finished,
                    job.state.as_str(),
                    job.error
                ],
            )
            .map_err(err)?;
        Ok(())
    }

    fn finish_job(
        &self,
        id: &str,
        state: JobStatus,
        error: Option<&str>,
        at: i64,
    ) -> StateResult<()> {
        self.conn()
            .execute(
                "UPDATE jobs SET state = ?2, error = ?3, finished = ?4 WHERE id = ?1",
                params![id, state.as_str(), error, at],
            )
            .map_err(err)?;
        Ok(())
    }

    fn job(&self, id: &str) -> StateResult<Option<JobRow>> {
        self.conn()
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_row,
            )
            .optional()
            .map_err(err)
    }

    fn jobs(&self, limit: usize) -> StateResult<Vec<JobRow>> {
        self.conn()
            .prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs ORDER BY started DESC, rowid DESC LIMIT ?1"
            ))
            .map_err(err)?
            .query_map([limit as i64], job_row)
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)
    }

    fn running_jobs(&self) -> StateResult<Vec<JobRow>> {
        self.conn()
            .prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'running' ORDER BY started"
            ))
            .map_err(err)?
            .query_map([], job_row)
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)
    }

    fn insert_link(&self, link: &LinkRow) -> StateResult<()> {
        self.conn()
            .execute(
                &format!(
                    "INSERT INTO links({LINK_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"
                ),
                params![
                    link.id,
                    link.kind.as_str(),
                    serde_json::to_string(&link.paths).unwrap_or_else(|_| "[]".into()),
                    link.zip,
                    link.name,
                    link.created,
                    link.expires,
                    link.bound_net,
                    link.size as i64,
                    link.max_bytes as i64,
                    link.bytes_used as i64,
                    link.committed as i64,
                    link.overwrite,
                    link.state.as_str(),
                    link.note
                ],
            )
            .map_err(err)?;
        Ok(())
    }

    fn link(&self, id: &str) -> StateResult<Option<LinkRow>> {
        self.conn()
            .query_row(
                &format!("SELECT {LINK_COLUMNS} FROM links WHERE id = ?1"),
                [id],
                link_row,
            )
            .optional()
            .map_err(err)
    }

    fn links(&self, limit: usize) -> StateResult<Vec<LinkRow>> {
        self.conn()
            .prepare(&format!(
                "SELECT {LINK_COLUMNS} FROM links ORDER BY created DESC, rowid DESC LIMIT ?1"
            ))
            .map_err(err)?
            .query_map([limit as i64], link_row)
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)
    }

    fn transition(
        &self,
        id: &str,
        from: &[LinkState],
        to: LinkState,
        note: Option<&str>,
    ) -> StateResult<bool> {
        let conn = self.conn();
        let current: Option<String> = conn
            .query_row("SELECT state FROM links WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .map_err(err)?;
        let Some(current) = current else {
            return Ok(false);
        };
        if !from.contains(&LinkState::parse(&current)) {
            return Ok(false);
        }
        let n = conn
            .execute(
                "UPDATE links SET state = ?2, note = coalesce(?3, note) WHERE id = ?1 AND state = ?4",
                params![id, to.as_str(), note, current],
            )
            .map_err(err)?;
        Ok(n == 1)
    }

    fn uses(&self, link_id: &str, limit: usize) -> StateResult<Vec<LinkUse>> {
        self.conn()
            .prepare(
                "SELECT at, ip, range, bytes, status FROM link_log WHERE link_id = ?1
                 ORDER BY at DESC, rowid DESC LIMIT ?2",
            )
            .map_err(err)?
            .query_map(params![link_id, limit as i64], |r| {
                Ok(LinkUse {
                    at: r.get(0)?,
                    ip: r.get(1)?,
                    range: r.get(2)?,
                    bytes: r.get::<_, i64>(3)? as u64,
                    status: r.get(4)?,
                })
            })
            .map_err(err)?
            .collect::<Result<_, _>>()
            .map_err(err)
    }

    fn reserved_bytes(&self, now: i64) -> StateResult<u64> {
        let n: i64 = self
            .conn()
            .query_row(
                "SELECT coalesce(sum(max(size - committed, 0)), 0) FROM links
                 WHERE kind = 'ul' AND state IN ('active', 'failed') AND expires > ?1",
                [now],
                |r| r.get(0),
            )
            .map_err(err)?;
        Ok(n.max(0) as u64)
    }

    fn staging_free_bytes(&self) -> StateResult<Option<u64>> {
        Ok(rustix::fs::statvfs(&self.staging)
            .ok()
            .map(|s| s.f_bavail.saturating_mul(s.f_frsize)))
    }

    fn audit(&self, at: i64, tool: &str, args: &str, result: &str) -> StateResult<()> {
        self.conn()
            .execute(
                "INSERT INTO audit(at, tool, args, result) VALUES (?1, ?2, ?3, ?4)",
                params![at, tool, args, result],
            )
            .map_err(err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cof-state-{}-{}",
            std::process::id(),
            chance_of_files_core::links::random_bytes::<4>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn link(id: &str, kind: LinkKind, size: u64) -> LinkRow {
        LinkRow {
            id: id.into(),
            kind,
            paths: vec!["/a b/c.txt".into()],
            zip: false,
            name: "c.txt".into(),
            created: 100,
            expires: 1000,
            bound_net: None,
            size,
            max_bytes: size * 3,
            bytes_used: 0,
            committed: 0,
            overwrite: false,
            state: LinkState::Active,
            note: None,
        }
    }

    #[test]
    fn links_bind_cap_and_reserve() {
        let dir = temp();
        let state = State::open(&dir, &dir.join("staging")).unwrap();
        state
            .insert_link(&link("d1", LinkKind::Download, 10))
            .unwrap();
        state
            .insert_link(&link("u1", LinkKind::Upload, 100))
            .unwrap();
        assert_eq!(state.link("d1").unwrap().unwrap().paths, ["/a b/c.txt"]);
        assert_eq!(state.bind("d1", "10.0.0.0/24").unwrap(), Bound::Ok);
        assert_eq!(state.bind("d1", "10.0.0.0/24").unwrap(), Bound::Ok);
        assert_eq!(
            state.bind("d1", "10.9.0.0/24").unwrap(),
            Bound::Elsewhere("10.0.0.0/24".into())
        );
        assert!(state.take_bytes("d1", 20).unwrap());
        assert!(!state.take_bytes("d1", 11).unwrap());
        state.give_back("d1", 5).unwrap();
        assert!(state.take_bytes("d1", 15).unwrap());
        assert_eq!(state.reserved_bytes(500).unwrap(), 100);
        state.set_committed("u1", 40).unwrap();
        assert_eq!(state.reserved_bytes(500).unwrap(), 60);
        assert_eq!(state.reserved_bytes(1000).unwrap(), 0, "expired");
        assert!(
            state
                .transition("u1", &[LinkState::Active], LinkState::Finishing, None)
                .unwrap()
        );
        assert!(
            !state
                .transition("u1", &[LinkState::Active], LinkState::Done, None)
                .unwrap()
        );
        assert_eq!(state.finishing().unwrap(), ["u1"]);
        state
            .log_use(
                "d1",
                &LinkUse {
                    at: 5,
                    ip: "10.0.0.1".into(),
                    range: "bytes=0-1".into(),
                    bytes: 2,
                    status: 206,
                },
            )
            .unwrap();
        assert_eq!(state.uses("d1", 10).unwrap().len(), 1);
        assert!(
            state
                .transition(
                    "d1",
                    &[LinkState::Active, LinkState::Failed],
                    LinkState::Revoked,
                    Some("revoked")
                )
                .unwrap()
        );
        assert_eq!(state.link("d1").unwrap().unwrap().state, LinkState::Revoked);
        assert!(state.staging_free_bytes().unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn jobs_and_sweeps() {
        let dir = temp();
        let state = State::open(&dir, &dir.join("staging")).unwrap();
        let job = JobRow {
            id: "j1".into(),
            backend_id: "7".into(),
            op: "move".into(),
            src: "/a".into(),
            dst: Some("/b".into()),
            started: 10,
            finished: None,
            state: JobStatus::Running,
            error: None,
        };
        state.insert_job(&job).unwrap();
        assert_eq!(state.running_jobs().unwrap(), std::slice::from_ref(&job));
        state.finish_job("j1", JobStatus::Done, None, 20).unwrap();
        assert!(state.running_jobs().unwrap().is_empty());
        assert_eq!(state.job("j1").unwrap().unwrap().finished, Some(20));
        state.insert_link(&link("u2", LinkKind::Upload, 5)).unwrap();
        assert_eq!(state.sweep(2000).unwrap(), ["u2"]);
        assert_eq!(state.link("u2").unwrap().unwrap().state, LinkState::Expired);
        state.audit(1, "move", "{}", "ok").unwrap();
        drop(state);
        // Reopening keeps everything.
        let state = State::open(&dir, &dir.join("staging")).unwrap();
        assert_eq!(state.jobs(10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
