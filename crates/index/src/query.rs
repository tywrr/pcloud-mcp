//! Reading the index: the audit queries, with keyset cursors.
//!
//! A cursor holds the sort key of the last row shown, a fingerprint of the query it belongs
//! to and the index generation. Pages continue strictly after that key, so rows are never
//! repeated or skipped while the index stays the same, and stay in order when a walk lands
//! between two pages.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use base64::Engine as _;
use chance_of_files_core::{
    Change, Filter, HashKind, IndexError, IndexResult, IndexedEntry, Policy, SizeGroup, SizeGroups,
    Sort, Totals, TypeRow, What, fold,
};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row};
use serde_json::json;

use crate::paths::{depth, under_range};

pub(crate) fn db_error(e: rusqlite::Error) -> IndexError {
    IndexError::Db(e.to_string())
}

/// Order terms: an SQL expression and whether it sorts descending. The last is unique.
type Order = Vec<(&'static str, bool)>;

fn entry_order(sort: Sort, files_only: bool) -> Order {
    // Files-only orders use bare columns so SQLite can walk the size and mtime indexes.
    let size = if files_only {
        "e.size"
    } else {
        "coalesce(t.bytes, e.size, 0)"
    };
    let newest = if files_only {
        "e.mtime"
    } else {
        "coalesce(t.newest, e.mtime)"
    };
    match sort {
        Sort::Name => vec![
            ("e.is_dir", true),
            ("e.name_fold", false),
            ("e.path", false),
        ],
        Sort::Path => vec![("e.path", false)],
        // Ties go the same way as the size or time, so an index on (size, path) serves both.
        Sort::Largest => vec![(size, true), ("e.path", true)],
        Sort::Smallest => vec![(size, false), ("e.path", false)],
        Sort::Newest => vec![(newest, true), ("e.path", true)],
        Sort::Oldest => vec![(newest, false), ("e.path", false)],
        Sort::Files => vec![("coalesce(t.files, 0)", true), ("e.path", true)],
    }
}

/// A query's cursor contents.
struct Cursor {
    keys: Vec<Value>,
}

fn fingerprint(parts: &impl Hash) -> String {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Integer(i) => json!(i),
        Value::Text(s) => json!(s),
        Value::Real(f) => json!(f),
        Value::Null | Value::Blob(_) => serde_json::Value::Null,
    }
}

fn from_json(value: &serde_json::Value) -> Option<Value> {
    Some(match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i),
            None => Value::Real(n.as_f64()?),
        },
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Bool(b) => Value::Integer(i64::from(*b)),
        _ => return None,
    })
}

pub(crate) fn encode_cursor(query: &str, generation_no: i64, keys: &[Value]) -> String {
    let body =
        json!({"q": query, "g": generation_no, "k": keys.iter().map(to_json).collect::<Vec<_>>()});
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body.to_string())
}

fn decode_cursor(cursor: Option<&str>, query: &str, tool: &str) -> IndexResult<Option<Cursor>> {
    let Some(cursor) = cursor.map(str::trim).filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    let bad = || {
        IndexError::Cursor(format!(
            "cursor is not one {tool} gave out for these same arguments; repeat the call without \
             it to start from the first page."
        ))
    };
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| bad())?;
    let body: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| bad())?;
    if body.get("q").and_then(|q| q.as_str()) != Some(query) {
        return Err(bad());
    }
    let keys = body
        .get("k")
        .and_then(|k| k.as_array())
        .ok_or_else(bad)?
        .iter()
        .map(from_json)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(bad)?;
    Ok(Some(Cursor { keys }))
}

/// `(k0 > ?) OR (k0 = ? AND k1 > ?) OR ...`, with `<` for descending terms.
fn after_clause(order: &Order, cursor: &Cursor, params: &mut Vec<Value>) -> IndexResult<String> {
    if cursor.keys.len() != order.len() {
        return Err(IndexError::Cursor(
            "cursor doesn't match this sort; start again without it.".into(),
        ));
    }
    if order.iter().all(|(_, desc)| *desc == order[0].1) {
        // One direction: a row-value comparison, which SQLite can answer from an index range.
        let mut slots = Vec::new();
        for key in &cursor.keys {
            params.push(key.clone());
            slots.push(format!("?{}", params.len()));
        }
        return Ok(format!(
            "({}) {} ({})",
            key_columns(order),
            if order[0].1 { "<" } else { ">" },
            slots.join(", ")
        ));
    }
    let mut alternatives = Vec::new();
    for i in 0..order.len() {
        let mut terms = Vec::new();
        for (j, (expr, _)) in order.iter().enumerate().take(i) {
            params.push(cursor.keys[j].clone());
            terms.push(format!("{expr} = ?{}", params.len()));
        }
        let (expr, desc) = order[i];
        params.push(cursor.keys[i].clone());
        terms.push(format!(
            "{expr} {} ?{}",
            if desc { "<" } else { ">" },
            params.len()
        ));
        alternatives.push(format!("({})", terms.join(" AND ")));
    }
    Ok(format!("({})", alternatives.join(" OR ")))
}

fn order_by(order: &Order) -> String {
    order
        .iter()
        .map(|(expr, desc)| format!("{expr}{}", if *desc { " DESC" } else { "" }))
        .collect::<Vec<_>>()
        .join(", ")
}

const ENTRY_COLUMNS: &str = "e.path, e.name, e.is_dir, e.size, e.mtime, e.ext, e.kind, \
                             e.remote_id, t.files, t.dirs, t.bytes, t.newest";
const ENTRY_COLUMN_COUNT: usize = 12;
/// The same columns for queries over files only, which have no totals: joining
/// `folder_totals` on a false condition would still scan it once per row.
const FILE_COLUMNS: &str = "e.path, e.name, e.is_dir, e.size, e.mtime, e.ext, e.kind, \
                            e.remote_id, NULL, NULL, NULL, NULL";

fn read_entry(row: &Row<'_>) -> rusqlite::Result<IndexedEntry> {
    let is_dir: bool = row.get(2)?;
    let mtime: i64 = row.get(4)?;
    let totals = match row.get::<_, Option<i64>>(8)? {
        Some(files) if is_dir => Some(Totals {
            files: files as u64,
            dirs: row.get::<_, i64>(9)? as u64,
            bytes: row.get::<_, i64>(10)? as u64,
            newest: row.get(11)?,
        }),
        _ if is_dir => Some(Totals::default()),
        _ => None,
    };
    Ok(IndexedEntry {
        path: row.get(0)?,
        name: row.get(1)?,
        is_dir,
        size: row.get::<_, Option<i64>>(3)?.map(|s| s.max(0) as u64),
        mtime: (mtime != 0).then_some(mtime),
        ext: row.get(5)?,
        kind: row.get(6)?,
        remote_id: row.get(7)?,
        totals,
        hashes: Vec::new(),
        cursor: String::new(),
    })
}

/// Runs an entry query whose SELECT ends with the order's key columns, and fills cursors.
fn entry_page(
    conn: &Connection,
    sql: &str,
    params: &[Value],
    order: &Order,
    query: &str,
    generation_no: i64,
    policy: &Policy,
) -> IndexResult<Vec<IndexedEntry>> {
    let mut stmt = conn.prepare_cached(sql).map_err(db_error)?;
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(db_error)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(db_error)? {
        let mut entry = read_entry(row).map_err(db_error)?;
        // Hidden rows are never stored; this only matters if the policy changed under a
        // running server.
        if !policy.visible(&entry.path) {
            continue;
        }
        let keys: Vec<Value> = (0..order.len())
            .map(|i| row.get::<_, Value>(ENTRY_COLUMN_COUNT + i))
            .collect::<Result<_, _>>()
            .map_err(db_error)?;
        entry.cursor = encode_cursor(query, generation_no, &keys);
        out.push(entry);
    }
    Ok(out)
}

pub(crate) fn generation(conn: &Connection) -> IndexResult<i64> {
    conn.query_row("SELECT value FROM meta WHERE key = 'generation'", [], |r| {
        r.get(0)
    })
    .map_err(db_error)
}

/// The path as stored, when the index has it under another case.
pub(crate) fn stored_path(conn: &Connection, path: &str) -> IndexResult<Option<String>> {
    if path == "/" {
        return Ok(Some("/".into()));
    }
    let exact: Option<String> = conn
        .query_row(
            "SELECT path FROM entries WHERE path = ?1 AND deleted_at IS NULL",
            [path],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?;
    if exact.is_some() {
        return Ok(exact);
    }
    // Case-insensitive for ASCII, then checked folded so "é" and "É" agree too.
    let want = fold(path);
    let candidates: Vec<String> = conn
        .prepare_cached(
            "SELECT path FROM entries WHERE path = ?1 COLLATE NOCASE AND deleted_at IS NULL LIMIT 20",
        )
        .map_err(db_error)?
        .query_map([path], |r| r.get(0))
        .map_err(db_error)?
        .collect::<Result<_, _>>()
        .map_err(db_error)?;
    Ok(candidates.into_iter().find(|c| fold(c) == want))
}

pub(crate) fn entry(conn: &Connection, path: &str) -> IndexResult<Option<IndexedEntry>> {
    let Some(path) = stored_path(conn, path)? else {
        return Ok(None);
    };
    if path == "/" {
        let totals = conn
            .query_row(
                "SELECT files, dirs, bytes, newest FROM folder_totals WHERE path = '/'",
                [],
                |r| {
                    Ok(Totals {
                        files: r.get::<_, i64>(0)? as u64,
                        dirs: r.get::<_, i64>(1)? as u64,
                        bytes: r.get::<_, i64>(2)? as u64,
                        newest: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)?
            .unwrap_or_default();
        return Ok(Some(IndexedEntry {
            path: "/".into(),
            is_dir: true,
            kind: "folder".into(),
            totals: Some(totals),
            ..IndexedEntry::default()
        }));
    }
    let sql = format!(
        "SELECT {ENTRY_COLUMNS}, h.sha1, h.md5, h.sha256 FROM entries e
         LEFT JOIN folder_totals t ON t.path = e.path
         LEFT JOIN hashes h ON h.remote_id = coalesce(e.remote_id, 'path:' || e.path)
              AND h.size IS e.size AND h.mtime = e.mtime
         WHERE e.path = ?1 AND e.deleted_at IS NULL"
    );
    conn.query_row(&sql, [&path], |row| {
        let mut entry = read_entry(row)?;
        entry.hashes = read_hashes(row, ENTRY_COLUMN_COUNT)?;
        Ok(entry)
    })
    .optional()
    .map_err(db_error)
}

fn read_hashes(row: &Row<'_>, at: usize) -> rusqlite::Result<Vec<(HashKind, String)>> {
    let mut out = Vec::new();
    for (i, kind) in [HashKind::Sha1, HashKind::Md5, HashKind::Sha256]
        .into_iter()
        .enumerate()
    {
        if let Some(value) = row.get::<_, Option<String>>(at + i)?
            && !value.is_empty()
        {
            out.push((kind, value));
        }
    }
    Ok(out)
}

pub(crate) fn children(
    conn: &Connection,
    policy: &Policy,
    folder: &str,
    sort: Sort,
    cursor: Option<&str>,
    limit: usize,
) -> IndexResult<Vec<IndexedEntry>> {
    let query = fingerprint(&("children", folder, sort.name()));
    let order = entry_order(sort, false);
    let after = decode_cursor(cursor, &query, "list_folder")?;
    let mut params = vec![Value::Text(folder.to_owned())];
    let mut sql = format!(
        "SELECT {ENTRY_COLUMNS}, {keys} FROM entries e LEFT JOIN folder_totals t ON t.path = e.path
         WHERE e.parent = ?1 AND e.deleted_at IS NULL",
        keys = key_columns(&order)
    );
    if let Some(after) = &after {
        sql.push_str(" AND ");
        sql.push_str(&after_clause(&order, after, &mut params)?);
    }
    sql.push_str(&format!(" ORDER BY {} LIMIT {limit}", order_by(&order)));
    entry_page(
        conn,
        &sql,
        &params,
        &order,
        &query,
        generation(conn)?,
        policy,
    )
}

pub(crate) fn child_counts(conn: &Connection, folder: &str) -> IndexResult<(u64, u64)> {
    conn.query_row(
        "SELECT coalesce(sum(is_dir), 0), coalesce(sum(1 - is_dir), 0) FROM entries
         WHERE parent = ?1 AND deleted_at IS NULL",
        [folder],
        |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
    )
    .map_err(db_error)
}

fn key_columns(order: &Order) -> String {
    order
        .iter()
        .map(|(expr, _)| *expr)
        .collect::<Vec<_>>()
        .join(", ")
}

/// WHERE terms (after `e.deleted_at IS NULL`) for a filter.
fn filter_terms(
    conn: &Connection,
    policy: &Policy,
    filter: &Filter,
    params: &mut Vec<Value>,
) -> IndexResult<String> {
    let mut sql = String::new();
    let push = |sql: &mut String, term: String| {
        sql.push_str(" AND ");
        sql.push_str(&term);
    };
    if filter.under != "/" {
        let (lo, hi) = under_range(&filter.under);
        params.push(Value::Text(lo));
        params.push(Value::Text(hi));
        push(
            &mut sql,
            format!(
                "e.path >= ?{} AND e.path < ?{}",
                params.len() - 1,
                params.len()
            ),
        );
    }
    match filter.what {
        What::Files => push(&mut sql, "e.is_dir = 0".into()),
        What::Folders => push(&mut sql, "e.is_dir = 1".into()),
        What::Any => {}
    }
    if let Some(name) = &filter.name {
        params.push(Value::Text(fold(name)));
        push(
            &mut sql,
            format!("instr(e.name_fold, ?{}) > 0", params.len()),
        );
    }
    if let Some(kind) = &filter.kind {
        params.push(Value::Text(kind.clone()));
        push(&mut sql, format!("e.kind = ?{}", params.len()));
    }
    if !filter.ext.is_empty() {
        let mut slots = Vec::new();
        for ext in &filter.ext {
            params.push(Value::Text(ext.trim_start_matches('.').to_lowercase()));
            slots.push(format!("?{}", params.len()));
        }
        push(&mut sql, format!("e.ext IN ({})", slots.join(", ")));
    }
    if let Some(min) = filter.min_size {
        params.push(Value::Integer(min.min(i64::MAX as u64) as i64));
        push(&mut sql, format!("e.size >= ?{}", params.len()));
    }
    if let Some(max) = filter.max_size {
        params.push(Value::Integer(max.min(i64::MAX as u64) as i64));
        push(&mut sql, format!("e.size <= ?{}", params.len()));
    }
    if let Some(after) = filter.after {
        params.push(Value::Integer(after));
        push(&mut sql, format!("e.mtime >= ?{}", params.len()));
    }
    if let Some(before) = filter.before {
        params.push(Value::Integer(before));
        push(
            &mut sql,
            format!("e.mtime < ?{} AND e.mtime > 0", params.len()),
        );
    }
    if filter.skip_search_off {
        for skip in policy.search_skip().paths() {
            let stored = stored_path(conn, skip)?.unwrap_or_else(|| skip.clone());
            let (lo, hi) = under_range(&stored);
            params.push(Value::Text(stored));
            params.push(Value::Text(lo));
            params.push(Value::Text(hi));
            let n = params.len();
            push(
                &mut sql,
                format!(
                    "NOT (e.path = ?{} OR (e.path >= ?{} AND e.path < ?{n}))",
                    n - 2,
                    n - 1
                ),
            );
        }
    }
    Ok(sql)
}

fn filter_fingerprint(tool: &str, filter: &Filter, sort: Sort) -> String {
    fingerprint(&(
        tool,
        &filter.under,
        &filter.name,
        &filter.kind,
        &filter.ext,
        filter.min_size,
        filter.max_size,
        filter.after,
        filter.before,
        filter.what as u8,
        filter.skip_search_off,
        sort.name(),
    ))
}

pub(crate) fn find(
    conn: &Connection,
    policy: &Policy,
    filter: &Filter,
    sort: Sort,
    cursor: Option<&str>,
    limit: usize,
) -> IndexResult<Vec<IndexedEntry>> {
    let tool = if filter.name.is_some() {
        "search_files"
    } else {
        "query_files"
    };
    let query = filter_fingerprint(tool, filter, sort);
    // Folder-only orders mean nothing for files; sort those by path.
    let sort = if filter.what == What::Files && sort == Sort::Files {
        Sort::Path
    } else {
        sort
    };
    let order = entry_order(sort, filter.what == What::Files);
    let after = decode_cursor(cursor, &query, tool)?;
    let mut params = Vec::new();
    let terms = filter_terms(conn, policy, filter, &mut params)?;
    let (columns, join) = if filter.what == What::Files {
        (FILE_COLUMNS, "")
    } else {
        (
            ENTRY_COLUMNS,
            "LEFT JOIN folder_totals t ON t.path = e.path",
        )
    };
    let mut sql = format!(
        "SELECT {columns}, {keys} FROM entries e {join} WHERE e.deleted_at IS NULL{terms}",
        keys = key_columns(&order)
    );
    if let Some(after) = &after {
        sql.push_str(" AND ");
        sql.push_str(&after_clause(&order, after, &mut params)?);
    }
    sql.push_str(&format!(" ORDER BY {} LIMIT {limit}", order_by(&order)));
    entry_page(
        conn,
        &sql,
        &params,
        &order,
        &query,
        generation(conn)?,
        policy,
    )
}

pub(crate) fn count(
    conn: &Connection,
    policy: &Policy,
    filter: &Filter,
) -> IndexResult<(u64, u64)> {
    let mut params = Vec::new();
    let terms = filter_terms(conn, policy, filter, &mut params)?;
    let sql = format!(
        "SELECT count(*), coalesce(sum(e.size), 0) FROM entries e WHERE e.deleted_at IS NULL{terms}"
    );
    conn.query_row(&sql, rusqlite::params_from_iter(params.iter()), |r| {
        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
    })
    .map_err(db_error)
}

pub(crate) fn folders(
    conn: &Connection,
    policy: &Policy,
    under: &str,
    max_depth: u32,
    sort: Sort,
    cursor: Option<&str>,
    limit: usize,
) -> IndexResult<Vec<IndexedEntry>> {
    let query = fingerprint(&("folders", under, max_depth, sort.name()));
    let order = entry_order(sort, false);
    let after = decode_cursor(cursor, &query, "folder_stats")?;
    let mut params = vec![Value::Integer(i64::from(depth(under) + max_depth))];
    let mut sql = format!(
        "SELECT {ENTRY_COLUMNS}, {keys} FROM entries e LEFT JOIN folder_totals t ON t.path = e.path
         WHERE e.is_dir = 1 AND e.deleted_at IS NULL
           AND length(e.path) - length(replace(e.path, '/', '')) <= ?1",
        keys = key_columns(&order)
    );
    if under != "/" {
        let (lo, hi) = under_range(under);
        params.push(Value::Text(lo));
        params.push(Value::Text(hi));
        sql.push_str(" AND e.path >= ?2 AND e.path < ?3");
    }
    if let Some(after) = &after {
        sql.push_str(" AND ");
        sql.push_str(&after_clause(&order, after, &mut params)?);
    }
    sql.push_str(&format!(" ORDER BY {} LIMIT {limit}", order_by(&order)));
    entry_page(
        conn,
        &sql,
        &params,
        &order,
        &query,
        generation(conn)?,
        policy,
    )
}

pub(crate) fn types(conn: &Connection, under: &str) -> IndexResult<Vec<TypeRow>> {
    let (lo, hi) = under_range(under);
    let mut stmt = conn
        .prepare_cached(
            "SELECT kind, ext, count(*), coalesce(sum(size), 0) FROM entries
             WHERE is_dir = 0 AND deleted_at IS NULL AND path >= ?1 AND path < ?2
             GROUP BY kind, ext",
        )
        .map_err(db_error)?;
    let rows = stmt
        .query_map([lo, hi], |r| {
            Ok(TypeRow {
                kind: r.get(0)?,
                ext: r.get(1)?,
                files: r.get::<_, i64>(2)? as u64,
                bytes: r.get::<_, i64>(3)? as u64,
            })
        })
        .map_err(db_error)?;
    rows.collect::<Result<_, _>>().map_err(db_error)
}

pub(crate) fn changes(
    conn: &Connection,
    policy: &Policy,
    under: &str,
    since: i64,
    source: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
) -> IndexResult<Vec<Change>> {
    let query = fingerprint(&("changes", under, since, source));
    let after = decode_cursor(cursor, &query, "recent_changes")?;
    let (lo, hi) = under_range(under);
    let mut sql = String::from(
        "SELECT seq, at, op, path, old_path, size, is_dir, source FROM changes
         WHERE at >= ?1 AND ((path >= ?2 AND path < ?3) OR (old_path >= ?2 AND old_path < ?3))",
    );
    let mut params = vec![Value::Integer(since), Value::Text(lo), Value::Text(hi)];
    if let Some(source) = source {
        params.push(Value::Text(source.to_owned()));
        sql.push_str(&format!(" AND source = ?{}", params.len()));
    }
    if let Some(after) = after {
        let Some(Value::Integer(seq)) = after.keys.first() else {
            return Err(IndexError::Cursor(
                "cursor is garbled; start again without it.".into(),
            ));
        };
        params.push(Value::Integer(*seq));
        sql.push_str(&format!(" AND seq < ?{}", params.len()));
    }
    sql.push_str(&format!(" ORDER BY seq DESC LIMIT {limit}"));
    let generation_no = generation(conn)?;
    let mut stmt = conn.prepare_cached(&sql).map_err(db_error)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            let seq: i64 = r.get(0)?;
            Ok(Change {
                seq,
                at: r.get(1)?,
                op: r.get(2)?,
                path: r.get(3)?,
                old_path: r.get(4)?,
                size: r.get::<_, Option<i64>>(5)?.map(|s| s.max(0) as u64),
                is_dir: r.get(6)?,
                source: r.get(7)?,
                cursor: encode_cursor(&query, generation_no, &[Value::Integer(seq)]),
            })
        })
        .map_err(db_error)?;
    let all: Vec<Change> = rows.collect::<Result<_, _>>().map_err(db_error)?;
    Ok(all
        .into_iter()
        .filter(|c| {
            policy.visible(&c.path) && c.old_path.as_deref().is_none_or(|p| policy.visible(p))
        })
        .collect())
}

/// Files that share a size with another file, at least `min_size`, below `under`.
const COLLISIONS: &str = "SELECT size FROM entries
     WHERE is_dir = 0 AND deleted_at IS NULL AND size >= ?1 AND path >= ?2 AND path < ?3
     GROUP BY size HAVING count(*) > 1";

pub(crate) fn size_groups(
    conn: &Connection,
    policy: &Policy,
    under: &str,
    min_size: u64,
    cursor: Option<&str>,
    max_files: usize,
) -> IndexResult<SizeGroups> {
    let query = fingerprint(&("duplicates", under, min_size));
    let after = decode_cursor(cursor, &query, "find_duplicates")?;
    let (lo, hi) = under_range(under);
    let min = min_size.min(i64::MAX as u64) as i64;
    let generation_no = generation(conn)?;
    let (total_groups, total_files, hashed_files) = conn
        .query_row(
            &format!(
                "SELECT count(DISTINCT e.size), count(*), count(h.remote_id) FROM entries e
                 LEFT JOIN hashes h ON h.remote_id = coalesce(e.remote_id, 'path:' || e.path)
                      AND h.size = e.size AND h.mtime = e.mtime
                 WHERE e.is_dir = 0 AND e.deleted_at IS NULL AND e.path >= ?2 AND e.path < ?3
                   AND e.size IN ({COLLISIONS})"
            ),
            rusqlite::params![min, lo, hi],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                ))
            },
        )
        .map_err(db_error)?;

    let below = match after.as_ref().and_then(|a| a.keys.first()) {
        Some(Value::Integer(size)) => *size,
        Some(_) => {
            return Err(IndexError::Cursor(
                "cursor is garbled; start again without it.".into(),
            ));
        }
        None => i64::MAX,
    };
    let sizes: Vec<i64> = conn
        .prepare_cached(&format!(
            "SELECT size FROM ({COLLISIONS}) WHERE size < ?4 ORDER BY size DESC LIMIT 1000"
        ))
        .map_err(db_error)?
        .query_map(rusqlite::params![min, lo, hi, below], |r| r.get(0))
        .map_err(db_error)?
        .collect::<Result<_, _>>()
        .map_err(db_error)?;

    let members_sql = format!(
        "SELECT {FILE_COLUMNS}, h.sha1, h.md5, h.sha256 FROM entries e
         LEFT JOIN hashes h ON h.remote_id = coalesce(e.remote_id, 'path:' || e.path)
              AND h.size = e.size AND h.mtime = e.mtime
         WHERE e.is_dir = 0 AND e.deleted_at IS NULL AND e.size = ?1
           AND e.path >= ?2 AND e.path < ?3
         ORDER BY e.path"
    );
    let mut stmt = conn.prepare_cached(&members_sql).map_err(db_error)?;
    let mut groups = Vec::new();
    let mut files = 0;
    for size in sizes {
        if files >= max_files && !groups.is_empty() {
            break;
        }
        let members: Vec<IndexedEntry> = stmt
            .query_map(rusqlite::params![size, lo, hi], |row| {
                let mut entry = read_entry(row)?;
                entry.hashes = read_hashes(row, ENTRY_COLUMN_COUNT)?;
                Ok(entry)
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?
            .into_iter()
            .filter(|e| policy.visible(&e.path))
            .collect();
        files += members.len();
        groups.push(SizeGroup {
            size: size as u64,
            files: members,
            cursor: encode_cursor(&query, generation_no, &[Value::Integer(size)]),
        });
    }
    Ok(SizeGroups {
        groups,
        total_groups,
        total_files,
        hashed_files,
    })
}

/// Collision files of at least `min_size` with no valid cached hash, largest first.
pub(crate) fn unhashed(
    conn: &Connection,
    min_size: u64,
    limit: usize,
) -> IndexResult<Vec<IndexedEntry>> {
    let min = min_size.min(i64::MAX as u64) as i64;
    let sql = format!(
        "SELECT {FILE_COLUMNS} FROM entries e
         WHERE e.is_dir = 0 AND e.deleted_at IS NULL
           AND e.size IN ({COLLISIONS})
           AND NOT EXISTS (SELECT 1 FROM hashes h
                           WHERE h.remote_id = coalesce(e.remote_id, 'path:' || e.path)
                             AND h.size = e.size AND h.mtime = e.mtime)
         ORDER BY e.size DESC, e.path LIMIT {limit}"
    );
    let (lo, hi) = under_range("/");
    let mut stmt = conn.prepare_cached(&sql).map_err(db_error)?;
    let rows = stmt
        .query_map(rusqlite::params![min, lo, hi], read_entry)
        .map_err(db_error)?;
    rows.collect::<Result<_, _>>().map_err(db_error)
}

/// How many collision files of at least `min_size` there are, and how many are hashed.
pub(crate) fn hash_progress(conn: &Connection, min_size: u64) -> IndexResult<(u64, u64)> {
    let min = min_size.min(i64::MAX as u64) as i64;
    let (lo, hi) = under_range("/");
    conn.query_row(
        &format!(
            "SELECT count(*), count(h.remote_id) FROM entries e
             LEFT JOIN hashes h ON h.remote_id = coalesce(e.remote_id, 'path:' || e.path)
                  AND h.size = e.size AND h.mtime = e.mtime
             WHERE e.is_dir = 0 AND e.deleted_at IS NULL AND e.size IN ({COLLISIONS})"
        ),
        rusqlite::params![min, lo, hi],
        |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
    )
    .map_err(db_error)
}

pub(crate) fn store_hashes(
    conn: &Connection,
    entry: &IndexedEntry,
    hashes: &[(HashKind, String)],
) -> IndexResult<()> {
    let get = |kind: HashKind| {
        hashes
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, v)| v.as_str())
    };
    let key = entry
        .remote_id
        .clone()
        .unwrap_or_else(|| format!("path:{}", entry.path));
    conn.execute(
        "INSERT OR REPLACE INTO hashes(remote_id, size, mtime, sha1, md5, sha256)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            key,
            entry.size.map(|s| s.min(i64::MAX as u64) as i64),
            entry.mtime.unwrap_or(0),
            get(HashKind::Sha1),
            get(HashKind::Md5),
            get(HashKind::Sha256)
        ],
    )
    .map_err(db_error)?;
    Ok(())
}
