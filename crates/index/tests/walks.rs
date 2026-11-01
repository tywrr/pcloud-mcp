//! Walks applied to the index: diffs, moves, guards, nested roots, own writes, failures.

mod common;

use chance_of_files_core::{Filter, Index, Policy, Sort, What};
use chance_of_files_index::WalkRow;
use common::{Tree, open};

fn no_policy() -> Policy {
    Policy::default()
}

fn paths(rows: &[chance_of_files_core::IndexedEntry]) -> Vec<String> {
    rows.iter().map(|e| e.path.clone()).collect()
}

fn sample() -> std::sync::Arc<Tree> {
    let tree = Tree::new();
    tree.file("/Docs/a.pdf", 100)
        .file("/Docs/b.txt", 20)
        .file("/Docs/old/c.docx", 30)
        .file("/Photos/2024/x.jpg", 1000)
        .file("/Photos/2024/y.jpg", 2000)
        .file("/readme.md", 5);
    tree
}

#[tokio::test]
async fn first_walk_fills_the_index_and_totals_without_logging_changes() {
    let f = open(sample(), &["/"], no_policy());
    f.walk("/").await;
    let roots = f.index.roots().unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].entries, 10, "{roots:?}");
    assert!(roots[0].as_of.is_some() && roots[0].last_error.is_none());

    let top = f.index.entry("/").unwrap().unwrap();
    let totals = top.totals.unwrap();
    assert_eq!((totals.files, totals.dirs, totals.bytes), (6, 4, 3155));
    let photos = f.index.entry("/photos").unwrap().unwrap();
    assert_eq!(photos.path, "/Photos", "lookups forgive case");
    assert_eq!(photos.totals.unwrap().bytes, 3000);
    assert_eq!(f.index.child_counts("/").unwrap(), (2, 1));
    assert!(f.index.changes("/", 0, None, None, 100).unwrap().is_empty());
}

#[tokio::test]
async fn later_walks_log_adds_removals_changes_and_moves() {
    let f = open(sample(), &["/"], no_policy());
    f.walk("/").await;
    f.tree.file("/Docs/new.md", 7);
    f.tree.remove("/Docs/old");
    f.tree.set_size("/Docs/a.pdf", 150, "2026-03-01T00:00:00Z");
    f.tree.rename("/Photos/2024", "/Photos/Trip 2024");
    f.walk("/").await;

    let changes = f.index.changes("/", 0, None, None, 100).unwrap();
    let lines: Vec<String> = changes
        .iter()
        .map(|c| format!("{} {} {:?}", c.op, c.path, c.old_path))
        .collect();
    assert!(
        lines.contains(&"added /Docs/new.md None".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"changed /Docs/a.pdf None".to_owned()),
        "{lines:?}"
    );
    // One line for the removed folder, not one per file in it.
    assert!(
        lines.contains(&"removed /Docs/old None".to_owned()),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("c.docx")), "{lines:?}");
    // The folder moved; its files moved with it and aren't listed again.
    assert!(
        lines.contains(&"moved /Photos/Trip 2024 Some(\"/Photos/2024\")".to_owned()),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("x.jpg")), "{lines:?}");
    assert_eq!(changes.len(), 4, "{lines:?}");

    assert!(f.index.entry("/Photos/Trip 2024/x.jpg").unwrap().is_some());
    assert!(f.index.entry("/Photos/2024/x.jpg").unwrap().is_none());
    assert_eq!(
        f.index.entry("/").unwrap().unwrap().totals.unwrap().bytes,
        3182
    );
}

#[tokio::test]
async fn a_walk_that_would_remove_too_much_is_held_until_confirmed() {
    let tree = Tree::new();
    for i in 0..100 {
        tree.file(&format!("/Many/f{i:03}.bin"), 10);
    }
    let f = open(tree, &["/"], no_policy());
    f.walk("/").await;
    for i in 0..20 {
        f.tree.remove(&format!("/Many/f{i:03}.bin"));
    }
    f.tree.file("/Many/new.bin", 1);
    f.walk("/").await;

    let root = &f.index.roots().unwrap()[0];
    let held = root.held.clone().expect("the removal is held");
    assert_eq!((held.removals, held.entries), (20, 101));
    // Adds still apply; the held files are still there.
    assert!(f.index.entry("/Many/new.bin").unwrap().is_some());
    assert!(f.index.entry("/Many/f000.bin").unwrap().is_some());

    let message = f.index.refresh(Some("/"), true).unwrap();
    assert!(
        message.contains("Applied the held removal of 20 entries"),
        "{message}"
    );
    assert!(f.index.entry("/Many/f000.bin").unwrap().is_none());
    assert!(f.index.roots().unwrap()[0].held.is_none());
    assert_eq!(f.index.roots().unwrap()[0].entries, 82);
}

#[tokio::test]
async fn a_small_removal_is_not_held_and_a_good_walk_clears_a_hold() {
    let tree = Tree::new();
    for i in 0..100 {
        tree.file(&format!("/Many/f{i:03}.bin"), 10);
    }
    let f = open(tree, &["/"], no_policy());
    f.walk("/").await;
    *f.tree.broken.lock().unwrap() = Vec::new();
    // A flaky listing: the whole folder seems gone.
    let saved: Vec<String> = (0..100).map(|i| format!("/Many/f{i:03}.bin")).collect();
    f.tree.remove("/Many");
    f.walk("/").await;
    assert!(f.index.roots().unwrap()[0].held.is_some());
    // Back again: the next walk sees everything and the hold goes away.
    for p in &saved {
        f.tree.file(p, 10);
    }
    f.walk("/").await;
    assert!(f.index.roots().unwrap()[0].held.is_none());
    assert_eq!(f.index.roots().unwrap()[0].entries, 101);
    // Three files of 101 is under 5%: applied at once.
    f.tree.remove("/Many/f000.bin");
    f.tree.remove("/Many/f001.bin");
    f.tree.remove("/Many/f002.bin");
    f.walk("/").await;
    assert!(f.index.roots().unwrap()[0].held.is_none());
    assert_eq!(f.index.roots().unwrap()[0].entries, 98);
}

#[tokio::test]
async fn nested_roots_split_the_walk_and_hidden_paths_are_never_fetched() {
    let tree = Tree::new();
    tree.file("/Backups/Immich/library/a.jpg", 500)
        .file("/Backups/Immich/library/b.jpg", 600)
        .file("/Backups/phone/c.zip", 50)
        .file("/Crypto Folder/secret.bin", 9)
        .file("/Docs/d.pdf", 10);
    let policy = Policy::new(&["/crypto folder"], &["/Backups/Immich"], &[] as &[&str]).unwrap();
    let f = open(tree, &["/", "/Backups/Immich"], policy);
    f.walk("/").await;
    let calls = f.tree.take_calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("Immich/") || c.ends_with("Immich")),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|c| c.contains("Crypto")), "{calls:?}");
    assert!(
        calls.contains(&"list /".to_owned()) && calls.contains(&"list /Backups".to_owned()),
        "{calls:?}"
    );

    // The child root's own folder comes from the parent walk; its contents don't yet.
    assert!(f.index.entry("/Backups/Immich").unwrap().is_some());
    assert!(f.index.entry("/Backups/Immich/library").unwrap().is_none());
    assert!(f.index.entry("/Crypto Folder").unwrap().is_none());

    f.walk("/Backups/Immich").await;
    let calls = f.tree.take_calls();
    assert_eq!(calls, ["walk /Backups/Immich"], "one recursive listing");
    let roots = f.index.roots().unwrap();
    assert_eq!(roots.iter().map(|r| r.entries).collect::<Vec<_>>(), [6, 3]);
    // Totals of / cover both roots, never the hidden folder.
    assert_eq!(
        f.index.entry("/").unwrap().unwrap().totals.unwrap().bytes,
        1160
    );

    // Walking / again leaves Immich's rows alone.
    f.walk("/").await;
    assert_eq!(f.index.roots().unwrap()[1].entries, 3);
    assert!(
        f.index
            .entry("/Backups/Immich/library/a.jpg")
            .unwrap()
            .is_some()
    );
    let all = f
        .index
        .find(
            &Filter {
                what: What::Any,
                ..Filter::under("/")
            },
            Sort::Path,
            None,
            100,
        )
        .unwrap();
    assert!(!paths(&all).iter().any(|p| p.contains("Crypto")), "{all:?}");
}

#[tokio::test]
async fn own_writes_and_deletes_win_over_a_walk_that_started_before_them() {
    let f = open(sample(), &["/"], no_policy());
    f.walk("/").await;
    let now = chance_of_files_core::time::now();

    // We wrote a file the listing (taken before) doesn't have yet.
    f.index
        .record_own_write(&WalkRow {
            path: "/Docs/written.txt".into(),
            is_dir: false,
            size: Some(3),
            mtime: Some(now),
            remote_id: Some("f999".into()),
        })
        .unwrap();
    // We deleted a file the listing still shows.
    f.index.record_own_delete("/Docs/b.txt").unwrap();
    assert!(f.index.entry("/Docs/b.txt").unwrap().is_none());

    // A walk that started before both: simulate by applying with an earlier start.
    let scope_started = now - 10;
    let rows: Vec<WalkRow> = {
        let all = f
            .index
            .find(
                &Filter {
                    what: What::Any,
                    ..Filter::under("/")
                },
                Sort::Path,
                None,
                1000,
            )
            .unwrap();
        let mut rows: Vec<WalkRow> = all
            .into_iter()
            .filter(|e| e.path != "/Docs/written.txt")
            .map(|e| WalkRow {
                path: e.path,
                is_dir: e.is_dir,
                size: e.size,
                mtime: e.mtime,
                remote_id: e.remote_id,
            })
            .collect();
        rows.push(WalkRow {
            path: "/Docs/b.txt".into(),
            is_dir: false,
            size: Some(20),
            mtime: None,
            remote_id: None,
        });
        rows
    };
    f.index
        .db()
        .apply_walk(
            &chance_of_files_index::WalkScope {
                root: "/",
                at: "/",
                started: scope_started,
                max_removal: 0.05,
                confirmed: false,
            },
            rows,
            1,
        )
        .unwrap();
    assert!(
        f.index.entry("/Docs/written.txt").unwrap().is_some(),
        "own write kept"
    );
    assert!(
        f.index.entry("/Docs/b.txt").unwrap().is_none(),
        "own delete not undone"
    );

    // A walk that starts later sees the truth: the tombstone goes, the write stays (it exists).
    f.tree.remove("/Docs/b.txt");
    f.tree.file("/Docs/written.txt", 3);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    f.walk("/").await;
    assert!(f.index.entry("/Docs/written.txt").unwrap().is_some());
    assert!(f.index.entry("/Docs/b.txt").unwrap().is_none());
    let tombstones: i64 = f
        .index
        .db()
        .writer()
        .query_row(
            "SELECT count(*) FROM entries WHERE deleted_at IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tombstones, 0);
    let own: Vec<_> = f
        .index
        .changes("/", 0, Some("own-write"), None, 10)
        .unwrap();
    assert_eq!(own.len(), 2, "{own:?}");
}

#[tokio::test]
async fn a_failed_walk_keeps_the_previous_generation_and_records_the_error() {
    let f = open(sample(), &["/"], no_policy());
    f.walk("/").await;
    let before = f.index.roots().unwrap()[0].clone();
    f.tree.broken.lock().unwrap().push("/".into());
    f.walk("/").await;
    let after = f.index.roots().unwrap()[0].clone();
    assert_eq!(after.entries, before.entries);
    assert_eq!(after.as_of, before.as_of);
    assert!(
        after
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("listing / failed")),
        "{after:?}"
    );
    assert!(f.index.entry("/Docs/a.pdf").unwrap().is_some());
    f.tree.broken.lock().unwrap().clear();
    f.walk("/").await;
    assert!(f.index.roots().unwrap()[0].last_error.is_none());
}

#[tokio::test]
async fn a_subtree_walk_only_touches_that_subtree() {
    let f = open(sample(), &["/"], no_policy());
    f.walk("/").await;
    f.tree.file("/Docs/new.md", 1);
    f.tree.remove("/readme.md");
    f.walk_at("/", "/Docs").await;
    assert_eq!(f.tree.take_calls().last().unwrap(), "walk /Docs");
    assert!(f.index.entry("/Docs/new.md").unwrap().is_some());
    assert!(
        f.index.entry("/readme.md").unwrap().is_some(),
        "outside the subtree"
    );
}

#[tokio::test]
async fn reopening_with_other_roots_and_policy_reconciles_rows() {
    let dir = common::temp_dir();
    let tree = Tree::new();
    tree.file("/Backups/Immich/a.jpg", 5)
        .file("/Private/p.txt", 1)
        .file("/x.txt", 1);
    {
        let f = common::open_in(dir.clone(), tree.clone(), &["/"], no_policy());
        f.walk("/").await;
        assert!(f.index.entry("/Private/p.txt").unwrap().is_some());
    }
    let policy = Policy::new(&["/Private"], &[] as &[&str], &[] as &[&str]).unwrap();
    let f = common::open_in(dir, tree, &["/", "/Backups/Immich"], policy);
    assert!(
        f.index.entry("/Private/p.txt").unwrap().is_none(),
        "now hidden: dropped"
    );
    assert!(f.index.entry("/Private").unwrap().is_none());
    let roots = f.index.roots().unwrap();
    assert_eq!(roots[1].path, "/Backups/Immich");
    assert_eq!(roots[1].entries, 1, "a.jpg moved to the new root");
    assert!(roots[1].as_of.is_none(), "but it hasn't been walked yet");
}

#[tokio::test]
async fn a_corrupt_index_file_is_moved_aside_and_rebuilt() {
    let dir = common::temp_dir();
    std::fs::write(
        dir.join("index.db"),
        b"this is not a database at all, not even close",
    )
    .unwrap();
    let f = common::open_in(dir.clone(), sample(), &["/"], no_policy());
    f.walk("/").await;
    assert_eq!(f.index.roots().unwrap()[0].entries, 10);
    let aside = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("index.db.corrupt-")
        });
    assert!(aside);
}

#[test]
fn roots_must_be_visible_and_distinct() {
    let tree = Tree::new();
    let policy = Policy::new(&["/Crypto Folder"], &[] as &[&str], &[] as &[&str]).unwrap();
    for bad in [&["/", "/crypto folder/x"][..], &["/a", "/A/"][..]] {
        let mut config = chance_of_files_index::IndexConfig::new(common::temp_dir());
        config.roots = common::roots(bad);
        let health = std::sync::Arc::new(chance_of_files_core::Health::new());
        assert!(
            chance_of_files_index::open(config, policy.clone(), health, tree.clone()).is_err(),
            "{bad:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_background_indexer_walks_due_roots_then_hashes_collisions() {
    let tree = Tree::new();
    tree.file_with("/a/one.bin", 2_000_000, "2026-01-01T00:00:00Z", "same")
        .file_with("/b/two.bin", 2_000_000, "2026-01-01T00:00:00Z", "same")
        .file_with("/b/three.bin", 2_000_000, "2026-01-01T00:00:00Z", "other")
        .file("/small.txt", 10);
    let mut config = chance_of_files_index::IndexConfig::new(common::temp_dir());
    config.roots = common::roots(&["/"]);
    let health = std::sync::Arc::new(chance_of_files_core::Health::new());
    let (index, indexer) =
        chance_of_files_index::open(config, Policy::default(), health, tree.clone()).unwrap();
    let running = tokio::spawn(indexer.run());
    let mut status = index.status().unwrap();
    for _ in 0..100 {
        if status.hash_candidates == 3 && status.hashed == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        status = index.status().unwrap();
    }
    running.abort();
    assert_eq!(
        (status.hash_candidates, status.hashed),
        (3, 3),
        "{status:?}"
    );
    assert!(status.roots[0].as_of.is_some());
    let hashes = tree
        .take_calls()
        .into_iter()
        .filter(|c| c.starts_with("hash "))
        .count();
    assert_eq!(hashes, 3);
}

#[tokio::test]
async fn a_subtree_walk_that_sees_held_entries_again_releases_them() {
    let tree = Tree::new();
    for i in 0..50 {
        tree.file(&format!("/A/f{i:03}.bin"), 10)
            .file(&format!("/B/f{i:03}.bin"), 10)
            .file(&format!("/Keep/f{i:03}.bin"), 10)
            .file(&format!("/Keep/g{i:03}.bin"), 10);
    }
    let f = open(tree, &["/"], no_policy());
    f.walk("/").await;
    // A flaky listing: /A and /B seem gone, so the full walk holds 102 removals.
    f.tree.remove("/A");
    f.tree.remove("/B");
    f.walk("/").await;
    let held = f.index.roots().unwrap()[0].held.clone().expect("held");
    assert_eq!(held.removals, 102);

    // /A is back; a walk of just /A sees it (and /A itself) again.
    for i in 0..50 {
        f.tree.file(&format!("/A/f{i:03}.bin"), 10);
    }
    f.walk_at("/", "/A").await;
    let held = f.index.roots().unwrap()[0]
        .held
        .clone()
        .expect("still held");
    assert_eq!(held.removals, 51, "only /B and its files are still held");

    // We wrote one of /B's files since: confirming leaves it alone.
    f.index
        .record_own_write(&WalkRow {
            path: "/B/f000.bin".into(),
            is_dir: false,
            size: Some(10),
            mtime: None,
            remote_id: None,
        })
        .unwrap();
    let message = f.index.refresh(Some("/"), true).unwrap();
    assert!(
        message.contains("Applied the held removal of 50 entries"),
        "{message}"
    );
    assert!(f.index.entry("/A").unwrap().is_some());
    assert!(f.index.entry("/A/f000.bin").unwrap().is_some());
    assert!(f.index.entry("/B/f000.bin").unwrap().is_some(), "own write");
    assert!(f.index.entry("/B/f001.bin").unwrap().is_none());
    assert!(f.index.roots().unwrap()[0].held.is_none());

    // Hold again, then a subtree walk that sees everything held clears the hold entirely.
    f.tree.remove("/A");
    f.walk("/").await;
    assert_eq!(
        f.index.roots().unwrap()[0]
            .held
            .as_ref()
            .map(|h| h.removals),
        Some(51)
    );
    for i in 0..50 {
        f.tree.file(&format!("/A/f{i:03}.bin"), 10);
    }
    f.walk_at("/", "/A").await;
    assert!(f.index.roots().unwrap()[0].held.is_none());
    let message = f.index.refresh(Some("/"), true).unwrap();
    assert!(message.contains("No held removals to confirm"), "{message}");
    assert!(f.index.entry("/A/f049.bin").unwrap().is_some());
}

#[tokio::test]
async fn an_interrupted_walk_is_retried_after_the_first_back_off() {
    let dir = common::temp_dir();
    let tree = sample();
    let now = chance_of_files_core::time::now();
    {
        let f = common::open_in(dir.clone(), tree.clone(), &["/"], no_policy());
        f.walk("/").await;
        // A finished walk is next due a whole interval (30 min) later...
        let due = f.index.roots().unwrap()[0].next_due.unwrap();
        assert!(due >= now + 1700, "{due} vs {now}");
        // ...but this one starts and the process stops before it is applied.
        // Well after the finished walk, even if that one ended a second or two after `now`.
        f.index.db().walk_starting("/", now + 10).unwrap();
    }
    let f = common::open_in(dir, tree, &["/"], no_policy());
    let due = f.index.roots().unwrap()[0].next_due.unwrap();
    // Two minutes after the interrupted start: not at once, not half an hour later.
    assert_eq!(due, now + 10 + 120, "due in {} s", due - now);
}

#[tokio::test]
async fn own_writes_and_deletes_in_the_same_second_as_the_walk_start_win() {
    let f = open(sample(), &["/"], no_policy());
    f.walk("/").await;
    let t = chance_of_files_core::time::now() + 5;
    let db = f.index.db();
    db.record_own_write(
        "/",
        &WalkRow {
            path: "/Docs/written.txt".into(),
            is_dir: false,
            size: Some(3),
            mtime: Some(t),
            remote_id: Some("f999".into()),
        },
        t,
    )
    .unwrap();
    db.record_own_delete("/Docs/b.txt", t).unwrap();
    // A walk that started in the same second listed the old state: b.txt, no written.txt.
    let rows: Vec<WalkRow> = f
        .index
        .find(
            &Filter {
                what: What::Any,
                ..Filter::under("/")
            },
            Sort::Path,
            None,
            1000,
        )
        .unwrap()
        .into_iter()
        .filter(|e| e.path != "/Docs/written.txt")
        .map(|e| WalkRow {
            path: e.path,
            is_dir: e.is_dir,
            size: e.size,
            mtime: e.mtime,
            remote_id: e.remote_id,
        })
        .chain([WalkRow {
            path: "/Docs/b.txt".into(),
            is_dir: false,
            size: Some(20),
            mtime: None,
            remote_id: None,
        }])
        .collect();
    db.apply_walk(
        &chance_of_files_index::WalkScope {
            root: "/",
            at: "/",
            started: t,
            max_removal: 0.05,
            confirmed: false,
        },
        rows,
        1,
    )
    .unwrap();
    assert!(f.index.entry("/Docs/written.txt").unwrap().is_some());
    assert!(f.index.entry("/Docs/b.txt").unwrap().is_none());
}

#[tokio::test]
async fn write_through_from_the_tools_side() {
    let f = open(sample(), &["/", "/Photos"], no_policy());
    f.walk("/").await;
    f.walk("/Photos").await;
    let index: &dyn Index = &*f.index;

    // A confirmed write shows at once, in the root that holds it, logged as our own.
    index
        .record_write(&chance_of_files_core::Entry {
            path: "/Photos/2024/z.jpg".into(),
            name: "z.jpg".into(),
            size: Some(7),
            modified: Some("2026-10-31T10:00:00Z".into()),
            id: Some("f77".into()),
            ..chance_of_files_core::Entry::default()
        })
        .unwrap();
    let z = index.entry("/photos/2024/z.jpg").unwrap().unwrap();
    assert_eq!((z.size, z.remote_id.as_deref()), (Some(7), Some("f77")));
    let changes = index.changes("/", 0, Some("own-write"), None, 10).unwrap();
    assert_eq!(changes[0].path, "/Photos/2024/z.jpg");

    // A delete takes the folder and everything under it away, whatever case it was asked in.
    index.record_delete("/docs/OLD").unwrap();
    assert!(index.entry("/Docs/old/c.docx").unwrap().is_none());
    assert!(index.entry("/Docs/old").unwrap().is_none());
    // Unknown paths are no error.
    index.record_delete("/nowhere").unwrap();

    // A re-walk after a job queues a walk of that folder; the next walk brings what the
    // drive has.
    f.tree.file("/Docs/new.txt", 4);
    index.rewalk("/Docs").unwrap();
    let status = index.roots().unwrap();
    assert!(
        status.iter().any(|r| r.path == "/" && r.queued),
        "{status:?}"
    );
    f.walk_at("/", "/Docs").await;
    assert!(index.entry("/Docs/new.txt").unwrap().is_some());
}
